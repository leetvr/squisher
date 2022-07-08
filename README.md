# Squisher

## What?
`squisher` is a program that takes a glTF or GLB file with PNG/JPG textures and produces a GLB file where the textures have been replaced with ASTC compressed ktx2 files, abusing the [KHR_texture_basisu](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_texture_basisu/README.md) extension. It probably isn't useful to you.

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

Which will produce `your_file_optimized.glb`.

## Requirements
In addition to [Rust](https://rustup.rs/), you must have the following programs installed. Tell `squisher` where they are by setting the environment variables `ASTC_PATH` and `KTX2KTK2_PATH` respectively:

- arm's [astc-encoder](https://github.com/ARM-software/astc-encoder)
- Khronos' [ktx2ktxt2](https://github.khronos.org/KTX-Software/ktxtools/ktx2ktx2.html)