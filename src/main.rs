use std::{
    borrow::Cow,
    collections::HashMap,
    mem,
    path::{Path, PathBuf},
    process::Command,
};

use gltf::json::{image::MimeType, Index};

const MAX_SIZE: u32 = 4096;

static BIN_ASTCENC: &str = "astcenc-avx2";
static BIN_KTX2KTX2: &str = "ktx2ktx2";
static BIN_TOKTX: &str = "toktx";

/// Check for cached versions. Mark this as false if the compression algorithm changes in some way.
const USE_CACHE: bool = true;

// Use KTX's toktx tool
const USE_TOKTX: bool = true;

fn main() {
    let file_name = std::env::args()
        .nth(1)
        .expect("You must provide the filename you'd like to squish.");
    squish(file_name)
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

pub fn squish<P: AsRef<Path>>(file_name: P) {
    let path = file_name.as_ref();
    println!("Squishing {}..", path.to_str().unwrap(),);
    let input = open(path);
    let optimized_glb = optimize(input);

    let mut output_path = path.to_path_buf();
    let stem = output_path.file_stem().unwrap().to_str().unwrap();
    output_path.set_file_name(format!("{}_squished.glb", stem));
    std::fs::write(&output_path, optimized_glb).unwrap();
    println!(
        "Squished file: {}! Enjoy: ✨",
        output_path.to_str().unwrap()
    )
}

fn optimize(input: Input) -> Vec<u8> {
    let mut image_map: HashMap<usize, Vec<u8>> = Default::default();
    // First, compress the images.
    // In order to do this, we need to have a bit of information about them first:
    let document = &input.document;
    for material in document.materials() {
        // Okiedokie. Each part of the material needs to be treated differently. Let's start with the easy stuff.
        let pbr = material.pbr_metallic_roughness();
        if let Some(base_colour) = pbr.base_color_texture() {
            let texture = base_colour.texture();
            let compressed = compress_texture(&texture, &input, TextureType::BaseColor);
            image_map.insert(texture.source().index(), compressed);
        }

        if let Some(metallic_roughness) = pbr.metallic_roughness_texture() {
            let texture = metallic_roughness.texture();
            let compressed =
                compress_texture(&texture, &input, TextureType::MetallicRoughnessOcclusion);
            image_map.insert(texture.source().index(), compressed);
        }

        if let Some(normal) = material.normal_texture() {
            let texture = normal.texture();
            let compressed = compress_texture(&texture, &input, TextureType::Normal);
            image_map.insert(texture.source().index(), compressed);
        }

        if let Some(emissive) = material.emissive_texture() {
            let texture = emissive.texture();
            let compressed = compress_texture(&texture, &input, TextureType::Emissive);
            image_map.insert(texture.source().index(), compressed);
        }

        if let Some(occlusion) = material.occlusion_texture() {
            let texture = occlusion.texture();
            let compressed =
                compress_texture(&texture, &input, TextureType::MetallicRoughnessOcclusion);
            image_map.insert(texture.source().index(), compressed);
        }
    }

    // Okay. Now that's done we need a new GLB file.
    create_glb_file(input, image_map)
}

fn create_glb_file(input: Input, image_map: HashMap<usize, Vec<u8>>) -> Vec<u8> {
    // Ugh, this is going to be disgusting.
    let mut new_blob: Vec<u8> = Vec::new();
    let blob = &input.blob;
    let mut new_buffer_views: Vec<gltf::json::buffer::View> = Vec::new();
    let mut new_root = input.document.into_json();

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

    let new_blob = to_padded_byte_vector(new_blob);
    let buffer_length = new_blob.len() as u32;
    let json_string = gltf::json::serialize::to_string(&new_root).expect("Serialization error");
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
    glb.to_vec().unwrap()
}

fn align_to_multiple_of_four(n: &mut u32) {
    *n = (*n + 3) & !3;
}

fn to_padded_byte_vector<T>(vec: Vec<T>) -> Vec<u8> {
    let byte_length = vec.len() * mem::size_of::<T>();
    let byte_capacity = vec.capacity() * mem::size_of::<T>();
    let alloc = vec.into_boxed_slice();
    let ptr = Box::<[T]>::into_raw(alloc) as *mut u8;
    let mut new_vec = unsafe { Vec::from_raw_parts(ptr, byte_length, byte_capacity) };
    while new_vec.len() % 4 != 0 {
        new_vec.push(0); // pad to multiple of four bytes
    }
    new_vec
}

fn compress_texture(texture: &gltf::Texture, input: &Input, texture_type: TextureType) -> Vec<u8> {
    println!("[SQUISHER] Compressing {texture_type:?}..");
    // Okay. First thing we need to do is get the path of the texture. If the source is *inside* the GLB, we'll have to write it to disk first.
    let (input_path, _original_size) = match texture.source().source() {
        gltf::image::Source::View { view, mime_type } => {
            // Right. Bytes are BYTES.
            let bytes = &input.blob[view.offset()..view.offset() + view.length()];
            let mut path = file_name(bytes);
            let (extension, format) = if mime_type == "image/jpeg" {
                ("jpg", image::ImageFormat::Jpeg)
            } else {
                ("png", image::ImageFormat::Png)
            };

            // Now that we've got said bytes, let's resize the image.
            let mut image = image::io::Reader::new(std::io::Cursor::new(bytes));
            image.set_format(format);
            let mut image = image.decode().unwrap();
            if image.height() > MAX_SIZE {
                println!(
                    "[SQUISHER] Image is too large! ({}x{}), resizing to {}x{}",
                    image.height(),
                    image.width(),
                    MAX_SIZE,
                    MAX_SIZE,
                );
                image = image.resize(MAX_SIZE, MAX_SIZE, image::imageops::Lanczos3);
            }

            path.set_extension(extension);

            image
                .save_with_format(&path, format)
                .unwrap_or_else(|e| panic!("Unable to write image to path {:?} - {:?}", path, e));

            (path, bytes.len())
        }
        gltf::image::Source::Uri { uri, .. } => {
            // Technically glTF supports images not stored on disk (eg. the interweb) so let's make sure it's a real path.
            let path = Path::new(uri);
            assert!(
                path.exists(),
                "Corrupted glTF file or unsupported URI path - {}",
                uri
            );
            let file = std::fs::read(path).unwrap();
            let destination = file_name(&file);
            std::fs::write(&destination, &file).unwrap();
            (destination, file.len())
        }
    };

    let mut output_path = input_path.clone();
    output_path.set_extension("ktx2");

    // This file has already been hashed!
    if output_path.exists() && USE_CACHE {
        println!("[SQUISHER] Returning pre-compressed file!");
    } else {
        compress_image(&input_path, &mut output_path, texture_type);
    }

    // Now slurp up the image:
    let file = std::fs::read(&output_path).expect("Unable to read output file!");
    println!("[SQUISHER] Tempfile is at {output_path:?}");

    file
}

fn compress_image(input_path: &PathBuf, output_path: &mut PathBuf, texture_type: TextureType) {
    let delete_result = std::fs::remove_file(&output_path);
    println!(
        "[SQUISHER] Deleting destination file, if it exists: {:?}",
        delete_result
    );

    if USE_TOKTX {
        toktx(input_path, output_path, texture_type);
    } else {
        output_path.set_extension("ktx");
        // Right. Call astcenc
        astc(input_path, output_path, texture_type);

        // Nice work. Now we need to take that ktx file and convert it to ktx2.
        ktx2ktx2(output_path);

        // OK. Hopefully that worked.
        output_path.set_extension("ktx2");
    }
}

#[allow(unused)]
fn ktx2ktx2(output_path: &PathBuf) {
    // This command produces no output when it works correctly.
    let _output = Command::new(BIN_KTX2KTX2)
        .arg(output_path)
        .output()
        .expect("Error calling ktx2ktx2");
}

#[allow(unused)]
fn astc(input_path: &PathBuf, output_path: &PathBuf, texture_type: TextureType) {
    // TODO: don't hardcode the path
    let mut astc_command = Command::new(BIN_ASTCENC);

    // Some textures need to be stored as linear data, some should be sRGB. atsc_enc lets us specify that.
    if texture_type.is_srgb() {
        astc_command.arg("-cs")
    } else {
        astc_command.arg("-cl")
    };

    // Specify the input and output paths.
    astc_command.arg(input_path).arg(output_path);

    // Specify the block size
    astc_command.arg(texture_type.block_size());

    // Specify the quality
    astc_command.arg("-verythorough");

    // Add any additional arguments, if neccessary.
    if texture_type == TextureType::Normal {
        astc_command.arg("-normal");
    }

    println!(
        "[SQUISHER] Running astcenc with args {:?}",
        astc_command.get_args().collect::<Vec<_>>()
    );

    let output = astc_command.output().unwrap();
    if output.status.success() {
        println!("{}", String::from_utf8(output.stdout).unwrap());
    } else {
        panic!("{}", String::from_utf8(output.stdout).unwrap());
    }
}

fn toktx(input_path: &PathBuf, output_path: &PathBuf, texture_type: TextureType) {
    let mut command = Command::new(BIN_TOKTX);
    command.args(["--encode", "astc", "--astc_blk_d"]);
    command.arg(texture_type.block_size());
    command.args(["--astc_quality", "thorough", "--genmipmap"]);

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

    println!(
        "[SQUISHER] Running toktx with args {:?}",
        command.get_args().collect::<Vec<_>>()
    );
    let output = command.output().unwrap();
    if output.status.success() {
        println!("{}", String::from_utf8(output.stdout).unwrap());
    } else {
        eprintln!(
            "[SQUISHER] Error running command with args {:?}",
            command.get_args().collect::<Vec<_>>()
        );
        panic!("{}", String::from_utf8(output.stderr).unwrap());
    }
}

// Create a temporary file. There's probably a better way to do this.
fn file_name(file_bytes: &[u8]) -> PathBuf {
    let hash = seahash::hash(file_bytes);
    let mut path = std::env::temp_dir();
    path.set_file_name(format!("squisher_temp_{}", hash));
    path
}

fn open(path: &Path) -> Input {
    let reader = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Unable to open file {}: {}", path.display(), e));
    match path.extension().and_then(|s| s.to_str()) {
        Some("gltf") => {
            todo!("gltf files are not currently supported, sorry!")
            // let gltf = gltf::Gltf::from_reader(reader).expect("Unable to open gltf file!");
            // let blob = gltf
            //     .blob
            //     .expect("Sorry, only glTF files with embedded binaries are supported");
            // Input {
            //     document: gltf.document,
            //     blob,
            // }
        }
        Some("glb") => {
            let glb = gltf::Glb::from_reader(reader).expect("Unable to open GLB file!");
            let json = gltf::json::Root::from_slice(&glb.json).unwrap();
            let document =
                gltf::Document::from_json(json).expect("Invalid JSON section of GLB file");
            let blob = glb.bin.expect("No data in GLB file").to_vec();
            Input { document, blob }
        }
        _ => panic!(
            "File does not have extension gltf or glb: {}",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // pub fn test_that_it_works_with_gltf() {
    //     // TODO - make sure that we delete the output file first.
    //     squish("test_data/BoxTextured.gltf");
    //     verify("test_data/BoxTextured_squished.glb");
    // }
    #[test]
    pub fn test_that_it_works_with_glb() {
        squish("test_data/BoxTexturedBinary.glb");
        verify("test_data/BoxTexturedBinary_squished.glb");
    }

    pub fn verify<P: AsRef<Path>>(p: P) {
        let path = p.as_ref();
        assert!(path.exists());
        let input = open(path);
        for image in input.document.images() {
            match image.source() {
                gltf::image::Source::View { view, .. } => {
                    // Get the image, then make sure it was compressed correctly.
                    let bytes = &input.blob[view.offset()..view.offset() + view.length()];
                    let reader = ktx2::Reader::new(bytes).unwrap();
                    let header = reader.header();
                    assert_eq!(header.format.unwrap(), ktx2::Format::ASTC_6x6_SRGB_BLOCK);
                    assert_eq!(header.level_count, 9);
                }
                _ => unreachable!(),
            }
        }
    }
}
