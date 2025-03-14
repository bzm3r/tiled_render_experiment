use bevy::{
    prelude::*,
    reflect::TypeUuid,
    render::{
        camera::{CameraProjection, RenderTarget, WindowOrigin},
        mesh::PrimitiveTopology,
        render_resource::{
            AsBindGroup, Extent3d, ShaderRef, TextureDescriptor, TextureDimension, TextureFormat,
            TextureUsages,
        },
        renderer::RenderQueue,
        view::RenderLayers,
    },
    sprite::{Anchor, Material2d, Material2dPlugin, MaterialMesh2dBundle},
};
use bevy_prototype_lyon::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::{
    get_grid_shape, DrawTileEvent, FlattenedElems, Layers, LibLayers, MainCamera,
    RenderingCompleteEvent, TileMap, TileMapLowerLeft,
};
use layout21::raw;

use bevy::render::camera::Viewport;
use std::time::{Duration, Instant};

pub const ALPHA: f32 = 0.1;
pub const WIDTH: f32 = 10.0;

pub const DOWNSCALING_PASS_LAYER: RenderLayers = RenderLayers::layer(1);
pub const ACCUMULATION_PASS_LAYER: RenderLayers = RenderLayers::layer(2);
pub const MAIN_CAMERA_LAYER: RenderLayers = RenderLayers::layer(3);

pub struct TiledRendererPlugin;

impl Plugin for TiledRendererPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(ShapePlugin)
            .add_plugin(Material2dPlugin::<PostProcessingMaterial>::default())
            .insert_resource({
                let (sender, receiver) = bounded::<()>(1);
                RenderingDoneChannel { sender, receiver }
            })
            .add_system(despawn_system)
            .add_system(spawn_shapes_system)
            .add_system(spawn_cameras_system.after(spawn_shapes_system));
        // .add_system(assets_debug_system);
    }
}

struct RenderingDoneChannel {
    sender: Sender<()>,
    receiver: Receiver<()>,
}

#[derive(Component, Clone)]
struct LyonShape;

#[derive(Component)]
struct TextureCam;

#[derive(Debug, Clone, Copy, Component)]
pub struct HiResTileMarker;

#[derive(Debug, Clone, Copy, Component)]
pub struct DownscaledTileMarker;

#[derive(Bundle)]
pub struct LyonShapeBundle {
    #[bundle]
    lyon: bevy_prototype_lyon::entity::ShapeBundle,
    marker: LyonShape,
}

fn spawn_shapes_system(
    mut commands: Commands,
    tilemap: Res<TileMap>,
    flattened_elems: Res<FlattenedElems>,
    lib_layers: Res<LibLayers>,
    layers: Res<Layers>,
    mut draw_ev: EventReader<DrawTileEvent>,
    mut existing_lyon_shapes: Query<
        (
            &mut bevy_prototype_lyon::entity::Path,
            &mut Transform,
            &mut Visibility,
        ),
        With<LyonShape>,
    >,
) {
    for DrawTileEvent(key) in draw_ev.iter() {
        let tile = tilemap.get(key).unwrap();

        // let read_lib_layers = lib_layers.read().unwrap();
        // let mut bundle_vec = Vec::with_capacity(tile.shapes.len());

        info!("Num shapes in this tile: {}", tile.shapes.len());

        let mut existing_shapes_iter = existing_lyon_shapes.iter_mut();
        for idx in tile.shapes.iter() {
            let el = &(**flattened_elems)[*idx];

            let layer = lib_layers
                .get(el.layer)
                .expect("This Element's LayerKey does not exist in this Library's Layers")
                .layernum as u8;

            let color = layers.get(&layer).unwrap();

            if let raw::Shape::Rect(r) = &el.inner {
                let raw::Rect { p0, p1 } = r;
                let xmin = p0.x / 4;
                let ymin = p0.y / 4;
                let xmax = p1.x / 4;
                let ymax = p1.y / 4;

                let lyon_poly = shapes::Polygon {
                    points: vec![
                        (xmin as f32, ymin as f32).into(),
                        (xmax as f32, ymin as f32).into(),
                        (xmax as f32, ymax as f32).into(),
                        (xmin as f32, ymax as f32).into(),
                    ],
                    closed: true,
                };

                let transform = Transform::from_translation(Vec3::new(0.0, 0.0, layer as f32));

                let lyon_shape = GeometryBuilder::build_as(
                    &lyon_poly,
                    DrawMode::Outlined {
                        fill_mode: FillMode {
                            color: *color.clone().set_a(ALPHA),
                            options: FillOptions::default(),
                        },
                        outline_mode: StrokeMode {
                            color: *color,
                            options: StrokeOptions::default().with_line_width(WIDTH),
                        },
                    },
                    transform,
                );

                let bundle = LyonShapeBundle {
                    lyon: lyon_shape,
                    marker: LyonShape,
                };

                if let Some((mut existing_path, mut existing_transform, mut vis)) =
                    existing_shapes_iter.next()
                {
                    *existing_path = bundle.lyon.path;
                    *existing_transform = bundle.lyon.transform;
                    vis.is_visible = true;
                } else {
                    commands.spawn_bundle(bundle);
                }
            }
        }
    }
}

fn spawn_cameras_system(
    mut commands: Commands,
    mut hires_texture: Local<Option<Handle<Image>>>,
    mut lores_texture: Local<Option<Handle<Image>>>,
    mut accumulation_texture: Local<Option<Handle<Image>>>,
    mut mesh_and_material: Local<Option<(Handle<Mesh>, Handle<PostProcessingMaterial>)>>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut post_processing_materials: ResMut<Assets<PostProcessingMaterial>>,
    render_queue: Res<RenderQueue>,
    tilemap: Res<TileMap>,
    lower_left_res: Res<TileMapLowerLeft>,
    mut draw_ev: EventReader<DrawTileEvent>,
    rendering_done_channel: Res<RenderingDoneChannel>,
    mut rendering_complete_ev: EventWriter<RenderingCompleteEvent>,
) {
    for DrawTileEvent(key) in draw_ev.iter() {
        let tile = tilemap.get(key).unwrap();

        // if tile.shapes.len() == 0 {
        //     rendering_complete_ev.send_default();
        //     continue;
        // }

        let accumulation_handle = if let Some(handle) = accumulation_texture.as_ref() {
            // info!("reusing texture");
            (*handle).clone()
        } else {
            let (grid_x, grid_y) = get_grid_shape(&tilemap);
            let size = Extent3d {
                width: grid_x * 32,
                height: grid_y * 32,
                ..default()
            };

            info!("accumulation texture size {size:?}");
            // panic!();

            // This is the texture that will be rendered to.
            let mut image = Image {
                texture_descriptor: TextureDescriptor {
                    label: None,
                    size,
                    dimension: TextureDimension::D2,
                    format: TextureFormat::Bgra8UnormSrgb,
                    mip_level_count: 1,
                    sample_count: 1,
                    usage: TextureUsages::TEXTURE_BINDING
                        | TextureUsages::COPY_DST
                        | TextureUsages::RENDER_ATTACHMENT,
                },
                ..default()
            };

            // fill image.data with zeroes
            image.resize(size);

            let handle = images.add(image);

            *accumulation_texture = Some(handle.clone());

            info!("creating new accumulation texture");

            commands
                .spawn_bundle(SpriteBundle {
                    sprite: Sprite {
                        custom_size: Some(Vec2::new(size.width as f32, size.height as f32)),
                        anchor: Anchor::BottomLeft,
                        ..default()
                    },
                    texture: handle.clone(),
                    transform: Transform::from_translation((0.0, 0.0, 1.0).into()),
                    ..default()
                })
                .insert(MAIN_CAMERA_LAYER);

            commands
                .spawn_bundle(SpriteBundle {
                    sprite: Sprite {
                        custom_size: Some(Vec2::new(
                            size.width as f32 * 1.1,
                            size.height as f32 * 1.1,
                        )),
                        anchor: Anchor::BottomLeft,
                        color: Color::rgb(1.0, 0.0, 0.0),
                        ..default()
                    },
                    transform: Transform::from_translation(
                        (-0.05 * size.width as f32, -0.05 * size.height as f32, 0.0).into(),
                    ),
                    ..default()
                })
                .insert(MAIN_CAMERA_LAYER);

            // println!("x {}, y {}", size.width, size.height);

            // panic!();

            handle
        };

        let hires_handle = if let Some(handle) = hires_texture.as_ref() {
            // info!("reusing texture");
            (*handle).clone()
        } else {
            let size = Extent3d {
                width: 4096,
                height: 4096,
                ..default()
            };

            // This is the texture that will be rendered to.
            let mut image = Image {
                texture_descriptor: TextureDescriptor {
                    label: None,
                    size,
                    dimension: TextureDimension::D2,
                    format: TextureFormat::Bgra8UnormSrgb,
                    mip_level_count: 1,
                    sample_count: 1,
                    usage: TextureUsages::TEXTURE_BINDING
                        | TextureUsages::COPY_DST
                        | TextureUsages::RENDER_ATTACHMENT,
                },
                ..default()
            };

            // fill image.data with zeroes
            image.resize(size);

            let handle = images.add(image);

            *hires_texture = Some(handle.clone());

            info!("creating new hires texture");

            handle
        };

        let lores_handle = if let Some(handle) = lores_texture.as_ref() {
            // info!("reusing texture");
            (*handle).clone()
        } else {
            let size = Extent3d {
                width: 32,
                height: 32,
                ..default()
            };

            // This is the texture that will be rendered to.
            let mut image = Image {
                texture_descriptor: TextureDescriptor {
                    label: None,
                    size,
                    dimension: TextureDimension::D2,
                    format: TextureFormat::Bgra8UnormSrgb,
                    mip_level_count: 1,
                    sample_count: 1,
                    usage: TextureUsages::TEXTURE_BINDING
                        | TextureUsages::COPY_DST
                        | TextureUsages::RENDER_ATTACHMENT,
                },
                ..default()
            };

            // fill image.data with zeroes
            image.resize(size);

            let handle = images.add(image);

            *lores_texture = Some(handle.clone());

            info!("creating new lores texture");

            handle
        };

        let tile_x = tile.extents.min().x - lower_left_res.x;
        let tile_y = tile.extents.min().y - lower_left_res.y;

        // info!("{tile_x}");
        // info!("{tile_y}");

        assert!(tile_x >= 0, "tile_x should be positive");
        assert!(tile_y >= 0, "tile_y should be positive");
        let x = tile_x / 4;
        let y = tile_y / 4;

        // info!("setting camera transform to {x}, {y}");

        let transform = Transform::from_translation(Vec3::new(x as f32, y as f32, 999.0));

        let mut camera = Camera2dBundle {
            camera_2d: Camera2d::default(),
            camera: Camera {
                target: RenderTarget::Image(hires_handle.clone()),
                ..default()
            },
            transform,
            ..default()
        };

        camera.projection.window_origin = WindowOrigin::BottomLeft;

        let size = images.get(&hires_handle).unwrap().size();

        // info!("{:?}", camera.projection);
        camera.projection.update(size.x as f32, size.y as f32);

        // info!("{:?}", camera.projection);

        commands.spawn_bundle(camera).insert(TextureCam);

        let (mesh_handle, material_handle) =
            if let Some((mesh_handle, material_handle)) = mesh_and_material.as_ref() {
                ((*mesh_handle).clone(), (*material_handle).clone())
            } else {
                let mut mesh = Mesh::new(PrimitiveTopology::TriangleList);
                mesh.insert_attribute(
                    Mesh::ATTRIBUTE_POSITION,
                    vec![[-1.0, 1.0, 0.0], [-1.0, -3.0, 0.0], [3.0, 1.0, 0.0]],
                );

                mesh.insert_attribute(
                    Mesh::ATTRIBUTE_UV_0,
                    vec![[0.0, 0.0], [0.0, 2.0], [2.0, 0.0]],
                );

                mesh.insert_attribute(
                    Mesh::ATTRIBUTE_NORMAL,
                    vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
                );

                let mesh_handle = meshes.add(mesh);

                // This material has the texture that has been rendered.
                let material_handle = post_processing_materials.add(PostProcessingMaterial {
                    source_image: hires_handle.clone(),
                });

                *mesh_and_material = Some((mesh_handle.clone(), material_handle.clone()));

                (mesh_handle, material_handle)
            };

        // Post processing 2d quad, with material using the render texture done by the main camera, with a custom shader.
        commands
            .spawn_bundle(MaterialMesh2dBundle {
                mesh: mesh_handle.into(),
                material: material_handle,
                ..default()
            })
            .insert(DOWNSCALING_PASS_LAYER);

        // // The downscaling pass camera.
        // commands
        //     .spawn_bundle(Camera2dBundle {
        //         camera: Camera {
        //             priority: 1,
        //             target: RenderTarget::Image(lores_handle.clone()),
        //             ..default()
        //         },
        //         ..Camera2dBundle::default()
        //     })
        //     .insert(DOWNSCALING_PASS_LAYER)
        //     .insert(TextureCam);

        let physical_position = UVec2::new((x / 128) as u32, (y / 128) as u32);

        info!("viewport: {physical_position:?}");

        // The accumulation pass camera.
        commands
            .spawn_bundle(Camera2dBundle {
                camera: Camera {
                    priority: 2,
                    target: RenderTarget::Image(accumulation_handle.clone()),
                    viewport: Some(Viewport {
                        // this is the same as the calculations we were doing to properly place the small texture's sprite
                        physical_position,
                        physical_size: UVec2::new(32, 32),
                        ..default()
                    }),
                    ..default()
                },
                ..Camera2dBundle::default()
            })
            .insert(DOWNSCALING_PASS_LAYER)
            .insert(TextureCam);

        // let x = (x / 64) as f32;
        // let y = (y / 64) as f32;

        // let transform = Transform::from_translation((x, y, 0.0).into());

        // commands
        //     .spawn_bundle(SpriteBundle {
        //         sprite: Sprite {
        //             custom_size: Some(Vec2::new(size.width as f32, size.height as f32)),
        //             anchor: Anchor::BottomLeft,
        //             ..default()
        //         },
        //         texture: downscaled_image_handle.clone(),
        //         transform,
        //         ..default()
        //     })
        //     .insert(MAIN_CAMERA_LAYER);

        // let mut main_camera_t = main_camera_q.single_mut();

        // main_camera_t.translation.x = x;
        // main_camera_t.translation.y = y;

        let s = rendering_done_channel.sender.clone();

        render_queue.on_submitted_work_done(move || {
            s.send(()).unwrap();
            // info!("work done event sent!");
        });
    }
}

fn despawn_system(
    mut commands: Commands,
    cam_q: Query<Entity, With<TextureCam>>,
    mut shape_q: Query<&mut Visibility, With<LyonShape>>,
    rendering_done_channel: Res<RenderingDoneChannel>,
    mut rendering_complete_ev: EventWriter<RenderingCompleteEvent>,
) {
    if let Ok(()) = rendering_done_channel.receiver.try_recv() {
        info!("RENDERING DONE");
        for cam in cam_q.iter() {
            // info!("despawn camera");
            commands.entity(cam).despawn();
        }
        for mut vis in shape_q.iter_mut() {
            // info!("despawn shape");
            // commands.entity(s).despawn();
            vis.is_visible = false;
        }
        rendering_complete_ev.send_default();
    }
}

fn assets_debug_system(q: Query<(Entity, &Handle<Image>)>, assets: Res<Assets<Image>>) {
    info!("{}", q.iter().len());

    for (e, h) in q.iter() {
        let i = assets.get(h).unwrap();
        info!("{} {h:?}", i.size());
    }
}

#[derive(AsBindGroup, TypeUuid, Clone)]
#[uuid = "bc2f08eb-a0fb-43f1-a908-54871ea597d5"]
struct PostProcessingMaterial {
    /// In this example, this image will be the result of the main camera.
    #[texture(0)]
    #[sampler(1)]
    source_image: Handle<Image>,
}

impl Material2d for PostProcessingMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/fullscreen.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "shaders/downscale.wgsl".into()
    }
}
