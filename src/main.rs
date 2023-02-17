use std::{
    borrow::Cow,
    collections::HashMap,
    hash::Hasher,
    io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use anyhow::{bail, Context, Error};
use clap::Parser;
use gltf::json::{image::MimeType, Index};

const MAX_SIZE: u32 = 4096;

static BIN_TOKTX: &str = "toktx";

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    /// The path to the file to process.
    input: PathBuf,

    /// Where to output the squished output.
    output: PathBuf,

    /// What texture format to use. Can be 'raw' or 'astc'.
    #[clap(long)]
    format: TextureFormat,

    /// Enables more verbose logging.
    #[clap(short, long)]
    verbose: bool,

    /// Disable the image cache, forcing all images to be reprocessed.
    #[clap(long)]
    no_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TextureFormat {
    Rgba8,
    Astc,
}

impl FromStr for TextureFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "raw" => Ok(Self::Rgba8),
            "astc" => Ok(Self::Astc),
            _ => bail!("unknown texture format '{s}', expected 'rgba8' or 'astc'"),
        }
    }
}

fn main() {
    let args = Args::parse();

    if let Err(err) = squish(args) {
        log::error!("Fatal error: {err:?}");
        std::process::exit(1);
    }
}

struct SquishContext {
    input: Input,
    use_cache: bool,
    texture_format: TextureFormat,
}

struct Input {
    document: gltf::Document,
    blob: Vec<u8>,
}

/// Which part of the glTF material model this texture is.
#[derive(PartialEq, Eq, Debug)]
enum TextureType {
    BaseColor,
    Normal,
    MetallicRoughnessOcclusion,
    Emissive,
}

impl TextureType {
    pub fn is_srgb(&self) -> bool {
        matches!(self, TextureType::BaseColor | TextureType::Emissive)
    }

    pub fn block_size(&self) -> &'static str {
        match self {
            // TextureType::MetallicRoughnessOcclusion => command.arg("6x6"),
            // TextureType::Emissive => command.arg("10x10"),
            TextureType::BaseColor | TextureType::Emissive => "6x6",
            _ => "4x4",
        }
    }
}

fn squish(args: Args) -> anyhow::Result<()> {
    configure_logging(args.verbose);

    let use_cache = !args.no_cache;

    log::info!("Squishing {}", args.input.display());
    let input = open(&args.input)?;
    let context = SquishContext {
        input,
        use_cache,
        texture_format: args.format,
    };

    let optimized_glb = context.optimize()?;
    fs_err::write(&args.output, optimized_glb)?;

    log::info!("Squished file: {}! ✨ Enjoy ✨", args.output.display());
    Ok(())
}

fn configure_logging(verbose: bool) {
    let filter = if verbose {
        "squisher=debug,warn"
    } else {
        "squisher=info,warn"
    };

    let log_env = env_logger::Env::default().default_filter_or(filter);

    // If logging is already configured (like running in a test), we should
    // suppress any issues initializing it.
    let _ = env_logger::Builder::from_env(log_env)
        .format_timestamp(None)
        .try_init();
}

impl SquishContext {
    fn optimize(self) -> anyhow::Result<Vec<u8>> {
        let mut image_map: HashMap<usize, Vec<u8>> = Default::default();

        // First, compress the images.
        // In order to do this, we need to have a bit of information about them first:
        let document = &self.input.document;
        for material in document.materials() {
            // Okiedokie. Each part of the material needs to be treated differently. Let's start with the easy stuff.
            let pbr = material.pbr_metallic_roughness();
            if let Some(base_colour) = pbr.base_color_texture() {
                let texture = base_colour.texture();
                let compressed = self.compress_texture(&texture, TextureType::BaseColor)?;
                image_map.insert(texture.source().index(), compressed);
            }

            if let Some(metallic_roughness) = pbr.metallic_roughness_texture() {
                let texture = metallic_roughness.texture();
                let compressed =
                    self.compress_texture(&texture, TextureType::MetallicRoughnessOcclusion)?;
                image_map.insert(texture.source().index(), compressed);
            }

            if let Some(normal) = material.normal_texture() {
                let texture = normal.texture();
                let compressed = self.compress_texture(&texture, TextureType::Normal)?;
                image_map.insert(texture.source().index(), compressed);
            }

            if let Some(emissive) = material.emissive_texture() {
                let texture = emissive.texture();
                let compressed = self.compress_texture(&texture, TextureType::Emissive)?;
                image_map.insert(texture.source().index(), compressed);
            }

            if let Some(occlusion) = material.occlusion_texture() {
                let texture = occlusion.texture();
                let compressed =
                    self.compress_texture(&texture, TextureType::MetallicRoughnessOcclusion)?;
                image_map.insert(texture.source().index(), compressed);
            }
        }

        // Okay. Now that's done we need a new GLB file.
        self.create_glb_file(image_map)
    }

    fn compress_texture(
        &self,
        texture: &gltf::Texture,
        texture_type: TextureType,
    ) -> anyhow::Result<Vec<u8>> {
        log::info!("Compressing {texture_type:?}...");

        // Okay. First thing we need to do is get the path of the texture. If the source is *inside* the GLB, we'll have to write it to disk first.
        let (input_path, _original_size) = match texture.source().source() {
            gltf::image::Source::View { view, mime_type } => {
                // Right. Bytes are BYTES.
                let bytes = &self.input.blob[view.offset()..view.offset() + view.length()];
                let mut path = file_name(self.texture_format, bytes);
                let (extension, format) = match mime_type {
                    "image/jpeg" => ("jpg", image::ImageFormat::Jpeg),
                    "image/png" => ("png", image::ImageFormat::Png),
                    _ => bail!("unsupported image MIME Type {mime_type}"),
                };

                // Now that we've got said bytes, let's resize the image.
                let mut image = image::io::Reader::new(io::Cursor::new(bytes));
                image.set_format(format);
                let mut image = image.decode()?;

                // TODO: Configurable max size for images.
                if image.height() > MAX_SIZE {
                    log::warn!(
                        "Image is too large! ({}x{}), resizing to {}x{}",
                        image.height(),
                        image.width(),
                        MAX_SIZE,
                        MAX_SIZE,
                    );
                    image = image.resize(MAX_SIZE, MAX_SIZE, image::imageops::Lanczos3);
                }

                path.set_extension(extension);

                image.save_with_format(&path, format)?;

                (path, bytes.len())
            }
            gltf::image::Source::Uri { uri, .. } => {
                // Technically glTF supports images not stored on disk (eg. the interweb) so let's make sure it's a real path.
                let path = Path::new(uri);
                anyhow::ensure!(
                    path.exists(),
                    "Corrupted glTF file or unsupported URI path - {}",
                    uri
                );
                let bytes = fs_err::read(path)?;
                let destination = file_name(self.texture_format, &bytes);
                fs_err::write(&destination, &bytes)?;
                (destination, bytes.len())
            }
        };

        let mut output_path = input_path.clone();
        output_path.set_extension("ktx2");

        // This file has already been hashed!
        if self.use_cache && output_path.exists() {
            log::info!("Returning pre-compressed file!");
        } else {
            compress_image(
                &input_path,
                &mut output_path,
                self.texture_format,
                texture_type,
            )?;
        }

        // Now slurp up the image:
        let file = fs_err::read(&output_path)?;
        log::debug!("Tempfile is at {}", output_path.display());

        Ok(file)
    }

    fn create_glb_file(self, image_map: HashMap<usize, Vec<u8>>) -> anyhow::Result<Vec<u8>> {
        // Ugh, this is going to be disgusting.
        let mut new_blob: Vec<u8> = Vec::new();
        let blob = &self.input.blob;
        let mut new_buffer_views: Vec<gltf::json::buffer::View> = Vec::new();
        let mut new_root = self.input.document.into_json();

        // First, we need to make a map that lets us find which image a bufferView points to, if any.
        let mut image_buffer_view_indices = HashMap::new();
        for (index, image) in new_root.images.iter().enumerate() {
            if let Some(image_view_index) = image.buffer_view {
                image_buffer_view_indices.insert(image_view_index.value(), index);
            }
        }

        // Next, go through each buffer view and write its data into our blob.
        for (index, view) in new_root.buffer_views.iter_mut().enumerate() {
            // Stash the CURRENT length (eg before we add to it) of the new blob
            let new_offset = new_blob.len();

            // Okay, this buffer view points to an image - we instead want to grab the bytes of the compressed image.
            let bytes = if let Some(image_index) = image_buffer_view_indices.get(&index) {
                image_map.get(image_index).unwrap()
            } else {
                // Not an image - just get the original data and return it as-is.
                let start = view.byte_offset.unwrap_or_default() as usize;
                let end = start + view.byte_length as usize;
                &blob[start..end]
            };

            // And write it into the new blob.
            new_blob.extend_from_slice(bytes);

            // Now create a new view and change its offset to reflect the new blob.
            let mut new_view = view.clone();
            new_view.byte_offset = Some(new_offset as _);
            new_view.byte_length = bytes.len() as _;
            new_buffer_views.push(new_view);
        }

        // OK. Now we need to update any images that had their uri set (bufferView and uri are mutually exclusive)
        for (index, image) in new_root.images.iter_mut().enumerate() {
            // Set the MIME type
            image.mime_type = Some(MimeType("image/ktx2".to_string()));

            // This image has already been processed, we can move on.
            if image.uri.is_none() {
                continue;
            }

            // Right. As before, stash the current length of the new blob
            let new_offset = new_blob.len();

            // Clear the URI
            image.uri = None;

            // Get the current length of the buffer views to use as an index
            let buffer_view_index = new_buffer_views.len();

            // Now write the new image data into the blob
            let image_data = image_map.get(&index).unwrap();
            new_blob.extend(image_data);

            // Create a new buffer view for this image
            let view = gltf::json::buffer::View {
                buffer: Index::new(0 as _),
                byte_length: image_data.len() as _,
                byte_offset: Some(new_offset as _),
                byte_stride: None,
                name: None,
                target: None,
                extensions: None,
                extras: Default::default(),
            };

            // And add it to the list
            new_buffer_views.push(view);

            // Finally, update the image to point to this new view.
            image.buffer_view = Some(Index::new(buffer_view_index as _));
        }

        // OK! We're done. Set the new root to use the new buffer views..
        new_root.buffer_views = new_buffer_views;

        // And make sure the buffer is set correctly.
        new_root.buffers = vec![gltf::json::Buffer {
            byte_length: new_blob.len() as _,
            name: None,
            uri: None,
            extensions: None,
            extras: Default::default(),
        }];

        // and.. that's it? Maybe? Hopefully.
        // This part is mostly lifted from https://github.com/gltf-rs/gltf/blob/master/examples/export/main.rs

        pad_byte_vector(&mut new_blob);
        let buffer_length = new_blob.len() as u32;
        let json_string = gltf::json::serialize::to_string(&new_root)?;
        let mut json_offset = json_string.len() as u32;
        align_to_multiple_of_four(&mut json_offset);

        let glb = gltf::binary::Glb {
            header: gltf::binary::Header {
                magic: *b"glTF",
                version: 2,
                length: json_offset + buffer_length,
            },
            bin: Some(Cow::Owned(new_blob)),
            json: Cow::Owned(json_string.into_bytes()),
        };

        // And we're done! Write the entire file to GLB.
        Ok(glb.to_vec()?)
    }
}

fn align_to_multiple_of_four(n: &mut u32) {
    *n = (*n + 3) & !3;
}

/// Pads the length of a byte vector to a multiple of four bytes.
fn pad_byte_vector(vec: &mut Vec<u8>) {
    while vec.len() % 4 != 0 {
        vec.push(0);
    }
}

fn compress_image(
    input_path: &Path,
    output_path: &mut PathBuf,
    texture_format: TextureFormat,
    texture_type: TextureType,
) -> anyhow::Result<()> {
    log::debug!("Deleting destination file if it exists");

    if let Err(err) = fs_err::remove_file(&output_path) {
        if err.kind() != io::ErrorKind::NotFound {
            let err = Error::new(err).context("failed to remove destination file");
            return Err(err);
        }
    }

    toktx(input_path, output_path, texture_format, texture_type)?;

    Ok(())
}

fn toktx(
    input_path: &Path,
    output_path: &Path,
    format: TextureFormat,
    texture_type: TextureType,
) -> anyhow::Result<()> {
    let mut command = Command::new(BIN_TOKTX);
    command.args([
        "--t2",        // Use KTX2 instead of KTX.
        "--genmipmap", // Generate mipmaps.
    ]);

    match format {
        TextureFormat::Rgba8 => {
            command.args(["--target_type", "RGBA"]);
        }
        TextureFormat::Astc => {
            command.args(["--encode", "astc", "--astc_blk_d"]);
            command.arg(texture_type.block_size());
            command.args(["--astc_quality", "thorough"]);
        }
    }

    if texture_type == TextureType::Normal {
        command.args(["--normal_mode", "--normalize"]);
    }

    command.arg("--assign_oetf");
    if texture_type.is_srgb() {
        command.arg("srgb");
    } else {
        command.arg("linear");
    }
    command.arg(output_path).arg(input_path);

    log::debug!(
        "Running {BIN_TOKTX} with args {:?}",
        command.get_args().collect::<Vec<_>>()
    );

    let output = command.output().context("failed to run toktx")?;
    if !output.status.success() {
        log::error!(
            "Error running toktx with args {:?}",
            command.get_args().collect::<Vec<_>>()
        );
        bail!("{}", String::from_utf8_lossy(&output.stderr));
    }

    Ok(())
}

// Create a temporary file. There's probably a better way to do this.
fn file_name(format: TextureFormat, file_bytes: &[u8]) -> PathBuf {
    let mut hasher = seahash::SeaHasher::new();
    hasher.write_u8(format as _);
    hasher.write(file_bytes);
    let hash = hasher.finish();

    let mut path = std::env::temp_dir();
    path.set_file_name(format!("squisher_temp_{}", hash));
    path
}

fn open(path: &Path) -> anyhow::Result<Input> {
    let reader = fs_err::File::open(path)?;

    match path.extension().and_then(|s| s.to_str()) {
        Some("gltf") => {
            bail!("gltf files are not currently supported, sorry!");
        }
        Some("glb") => {
            let glb = gltf::Glb::from_reader(reader).context("unable to parse GLB file")?;
            let json = gltf::json::Root::from_slice(&glb.json)?;
            let document = gltf::Document::from_json(json).context("invalid JSON in GLB file")?;
            let blob = glb.bin.context("no data in GLB file")?.into_owned();

            Ok(Input { document, blob })
        }
        _ => {
            bail!(
                "File does not have extension gltf or glb: {}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glb_astc() {
        let args = Args {
            input: "test_data/BoxTexturedBinary.glb".into(),
            output: "test_output/BoxTexturedBinary_astc.glb".into(),
            format: TextureFormat::Astc,
            verbose: true,
            no_cache: true,
        };

        let verification = VerifyArgs {
            path: "test_output/BoxTexturedBinary_astc.glb",
            format: ktx2::Format::ASTC_6x6_SRGB_BLOCK,
            mip_level_count: 9,
        };

        fs_err::create_dir_all("test_output").unwrap();
        squish(args).unwrap();
        verify(verification);
    }

    #[test]
    fn glb_rgba8() {
        let args = Args {
            input: "test_data/BoxTexturedBinary.glb".into(),
            output: "test_output/BoxTexturedBinary_raw.glb".into(),
            format: TextureFormat::Rgba8,
            verbose: true,
            no_cache: true,
        };

        let verification = VerifyArgs {
            path: "test_output/BoxTexturedBinary_raw.glb",
            format: ktx2::Format::R8G8B8A8_SRGB,
            mip_level_count: 9,
        };

        fs_err::create_dir_all("test_output").unwrap();
        squish(args).unwrap();
        verify(verification);
    }

    struct VerifyArgs {
        path: &'static str,
        format: ktx2::Format,
        mip_level_count: u32,
    }

    fn verify(args: VerifyArgs) {
        let path: &Path = args.path.as_ref();
        assert!(path.exists());

        let input = open(path).unwrap();
        for image in input.document.images() {
            match image.source() {
                gltf::image::Source::View { view, .. } => {
                    // Get the image, then make sure it was compressed correctly.
                    let bytes = &input.blob[view.offset()..view.offset() + view.length()];
                    let reader = ktx2::Reader::new(bytes).unwrap();
                    let header = reader.header();

                    assert_eq!(header.format, Some(args.format));
                    assert_eq!(header.level_count, args.mip_level_count);
                }
                _ => unreachable!(),
            }
        }
    }
}
