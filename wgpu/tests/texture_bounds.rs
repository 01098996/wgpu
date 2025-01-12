//! Tests for texture copy bounds checks.

use crate::common::{initialize_test, TestParameters};
use std::num::NonZeroU32;

#[test]
fn bad_copy_origin() {
    fn try_origin(origin: wgpu::Origin3d, should_panic: bool) {
        let mut parameters = TestParameters::default();
        if should_panic {
            parameters = parameters.failure();
        }

        initialize_test(parameters, |ctx| {
            let texture = ctx.device.create_texture(&TEXTURE_DESCRIPTOR);
            let data = vec![255; BUFFER_SIZE as usize];
            ctx.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &texture,
                    mip_level: 0,
                    origin,
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                BUFFER_COPY_LAYOUT,
                TEXTURE_SIZE,
            );
        });
    }

    try_origin(wgpu::Origin3d { x: 0, y: 0, z: 0 }, false);
    try_origin(wgpu::Origin3d { x: 1, y: 0, z: 0 }, true);
    try_origin(wgpu::Origin3d { x: 0, y: 1, z: 0 }, true);
    try_origin(wgpu::Origin3d { x: 0, y: 0, z: 1 }, true);
}

const TEXTURE_SIZE: wgpu::Extent3d = wgpu::Extent3d {
    width: 64,
    height: 64,
    depth_or_array_layers: 1,
};

const TEXTURE_DESCRIPTOR: wgpu::TextureDescriptor = wgpu::TextureDescriptor {
    label: Some("CopyOrigin"),
    size: TEXTURE_SIZE,
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format: wgpu::TextureFormat::Rgba8UnormSrgb,
    usage: wgpu::TextureUsages::COPY_DST.union(wgpu::TextureUsages::COPY_SRC),
};

const BYTES_PER_PIXEL: u32 = 4;

const BUFFER_SIZE: u32 = TEXTURE_SIZE.width * TEXTURE_SIZE.height * BYTES_PER_PIXEL;

const BUFFER_COPY_LAYOUT: wgpu::ImageDataLayout = wgpu::ImageDataLayout {
    offset: 0,
    bytes_per_row: NonZeroU32::new(TEXTURE_SIZE.width * BYTES_PER_PIXEL),
    rows_per_image: None,
};
