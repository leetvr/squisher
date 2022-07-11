use std::{
    borrow::Cow,
    collections::HashMap,
    mem,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use gltf::json::{extensions::texture::TextureBasisu, image::MimeType, Index};

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
enum TextureType {
    BaseColor,
    Normal,
    MetallicRoughness,
    Occlusion,
    Emissive,
}

impl TextureType {
    pub fn is_srgb(&self) -> bool {
        match self {
            TextureType::BaseColor | TextureType::Emissive => true,
            _ => false,
        }
    }

    pub fn swizzle(&self, command: &mut Command) {
        match self {
            TextureType::BaseColor | TextureType::Emissive => command.arg("-esw").arg("rgb1"),
            TextureType::Normal => command.arg("-normal"),
            // each call to `arg` returns the command again, so just do the same to keep the return types happy
            _ => command,
        };
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
    std::fs::write(&output_path, &optimized_glb).unwrap();
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
            let compressed = compress_texture(&texture, &input, TextureType::MetallicRoughness);
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
            let compressed = compress_texture(&texture, &input, TextureType::Occlusion);
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
            image_map.get(&image_index).unwrap()
        } else {
            // OK. Now we need to get the data this view refers to.
            let start = view.byte_offset.unwrap() as usize;
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

    // Next, we need to update the textures
    for texture in new_root.textures.iter_mut() {
        // Stash the original image index
        let original_index = texture.source.unwrap();

        // Now erase it
        texture.source = None;

        // Now set the extension - this is how we smuggle our ktx2 file into the glb (spooky laughter)
        let basis_u = TextureBasisu {
            source: original_index,
        };
        let extension = gltf::json::extensions::texture::Texture {
            texture_basisu: Some(basis_u),
        };
        texture.extensions = Some(extension)
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

    // Now declare that we're using the KHR_texture_basisu extension (albeit incorrectly)
    new_root.extensions_required = vec!["KHR_texture_basisu".to_string()];
    new_root.extensions_used = vec!["KHR_texture_basisu".to_string()];

    // and.. that's it? Maybe? Hopefully.
    // This part is mostly lifted from https://github.com/gltf-rs/gltf/blob/master/examples/export/main.rs
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
        bin: Some(Cow::Owned(to_padded_byte_vector(new_blob))),
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
    // Okay. First thing we need to do is get the path of the texture. If the source is *inside* the GLB, we'll have to write it to disk first.
    let input_path = match texture.source().source() {
        gltf::image::Source::View { view, mime_type } => {
            // Right. Bytes are BYTES.
            let bytes = &input.blob[view.offset()..view.offset() + view.length()];
            let mut path = tmp_file();
            let extension = if mime_type == "image/jpeg" {
                "jpg"
            } else {
                "png"
            };
            path.set_extension(extension);
            std::fs::write(&path, bytes).unwrap();
            path
        }
        gltf::image::Source::Uri { uri, .. } => {
            // Technically glTF supports images not stored on disk (eg. the interweb) so let's make sure it's a real path.
            let path = Path::new(uri);
            assert!(
                path.exists(),
                "Corrupted glTF file or unsupported URI path - {}",
                uri
            );
            let destination = tmp_file();
            std::fs::copy(path, destination).unwrap();
            tmp_file()
        }
    };

    let mut output_path = input_path.clone();
    output_path.set_extension("ktx");

    // Right. Next, call astc encoder
    astc(input_path, &output_path, texture_type);

    // Nice work. Now we need to take that ktx file and convert it to ktx2.
    ktx2ktx2(&output_path);

    // OK. Hopefully that worked.
    output_path.set_extension("ktx2");

    // Now slurp up the image:
    std::fs::read(output_path).expect("Unable to read output file!")
}

// TODO: don't hardcode the path
fn ktx2ktx2(output_path: &PathBuf) {
    let ktx2ktx2_path = r#"C:\Program Files\KTX-Software\bin\ktx2ktx2.exe"#;
    // This command produces no output when it works correctly.
    let _output = Command::new(ktx2ktx2_path)
        .arg(output_path)
        .output()
        .expect("Error calling ktx2ktx2");
}

// TODO: don't hardcode the path
fn astc(input_path: PathBuf, output_path: &PathBuf, texture_type: TextureType) {
    let astc_path = r#"C:\Users\kanem\Downloads\astcenc-3.7-windows-x64\astcenc\astcenc-avx2.exe"#;
    let mut astc_command = Command::new(astc_path);

    // Some textures need to be stored as linear data, some should be sRGB. atsc_enc lets us specify that.
    if texture_type.is_srgb() {
        astc_command.arg("-cs")
    } else {
        astc_command.arg("-cl")
    };

    // Specify the input and output paths.
    astc_command.arg(input_path).arg(output_path);

    // Specify the block size
    astc_command.arg("8x8");

    // Specify the quality
    astc_command.arg("-thorough");

    // Add any additional swizzle parameters, if required
    texture_type.swizzle(&mut astc_command);

    // println!(
    //     "Calling astc with {:#?} {:#?}",
    //     astc_command.get_program(),
    //     astc_command.get_args()
    // );
    let _output = astc_command.output().unwrap();
    // println!("ASTC Output: {:#?}", output);
}

// Create a temporary file. There's probably a better way to do this.
fn tmp_file() -> PathBuf {
    let mut path = std::env::temp_dir();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    path.set_file_name(format!("squisher_temp_{}", now));
    path
}

fn open(path: &Path) -> Input {
    let reader = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Unable to open file {}: {}", path.display(), e));
    match path.extension().map(|s| s.to_str()).flatten() {
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
    #[test]
    pub fn test_that_it_works_with_gltf() {
        // TODO - make sure that we delete the output file first.
        squish("test_data/BoxTextured.gltf");
        verify("test_data/BoxTextured_squished.glb");
    }

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
                    let format = reader.header().format.unwrap();
                    assert_eq!(format, ktx2::Format::ASTC_8x8_SRGB_BLOCK);
                }
                _ => unreachable!(),
            }
        }
    }
}
