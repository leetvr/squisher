# Squisher

## What?
`squisher` is a program that takes a glTF or GLB file with PNG/JPG textures and produces a GLB file where the textures have been replaced with ASTC compressed ktx2 files, *explicitly breaking the glTF spec*: [source](https://www.khronos.org/registry/glTF/specs/2.0/glTF-2.0.html#_image_mimetype).

If you want your assets to use optimised textures but don't want to pay the runtime transcoding cost of [KHR_texture_basisu](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_texture_basisu/README.md), this tool is for you.

## Why?
Because [hotham](https://github.com/leetvr/hotham) works best with compressed textures, and doing this stuff by hand is time consuming. It takes *literal minutes*. And we all know that [a watched pot never boils](https://www.youtube.com/watch?v=eTFBxp0VW9M).

## How?
Running `squisher` is easy:

````bash
cargo run your_file.gltf
````

OR

````bash
cargo run your_file.glb
````

Which will produce `your_file_squished.glb`.

## Requirements
 > note: this part is currently a lie, paths to these tools is hardcoded
 
In addition to [Rust](https://rustup.rs/), you must have the following programs installed. Tell `squisher` where they are by setting the environment variables `ASTC_PATH` and `KTX2KTK2_PATH` respectively:

- arm's [astc-encoder](https://github.com/ARM-software/astc-encoder)
- Khronos' [ktx2ktxt2](https://github.khronos.org/KTX-Software/ktxtools/ktx2ktx2.html)
