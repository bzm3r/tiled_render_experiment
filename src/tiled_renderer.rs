use bevy::{
    core_pipeline::clear_color::ClearColorConfig,
    prelude::*,
    reflect::TypeUuid,
    render::{
        camera::{CameraProjection, RenderTarget, Viewport, WindowOrigin},
        mesh::PrimitiveTopology,
        render_resource::{
            AsBindGroup, Extent3d, ShaderRef, TextureDescriptor, TextureDimension, TextureFormat,
            TextureUsages,
        },
        renderer::RenderQueue,
    },
    sprite::{Anchor, Material2d, Material2dPlugin, MaterialMesh2dBundle},
};
use bevy_prototype_lyon::prelude::*;
use crossbeam_channel::bounded;
use std::time::Duration;

use crate::{
    get_grid_shape,
    types::{
        DrawTileEvent, FlattenedElems, Layers, LibLayers, LyonShape, LyonShapeBundle,
        RenderingCompleteEvent, RenderingDoneChannel, TextureCam, TileMap, TileMapLowerLeft,
        ACCUMULATION_CAMERA_PRIORITY, ALPHA, DOWNSCALING_PASS_LAYER, MAIN_CAMERA_LAYER, WIDTH,
    },
};

use layout21::raw::{self, Element};

pub struct TiledRendererPlugin;

#[derive(StageLabel)]
enum TiledRenderStage {
    SpawnCameras,
    SpawnShapes,
    Despawn,
}

impl Plugin for TiledRendererPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugin(ShapePlugin)
            .add_plugin(Material2dPlugin::<PostProcessingMaterial>::default())
            .insert_resource({
                let (sender, receiver) = bounded::<()>(1);
                RenderingDoneChannel { sender, receiver }
            })
            .add_stage_after(
                CoreStage::Update,
                TiledRenderStage::SpawnCameras,
                SystemStage::parallel(),
            )
            .add_stage_after(
                TiledRenderStage::SpawnCameras,
                TiledRenderStage::SpawnShapes,
                SystemStage::parallel(),
            )
            .add_stage_after(
                TiledRenderStage::SpawnShapes,
                TiledRenderStage::Despawn,
                SystemStage::parallel(),
            )
            .add_system_to_stage(TiledRenderStage::SpawnCameras, spawn_cameras_system)
            .add_system_to_stage(TiledRenderStage::SpawnShapes, spawn_shapes_system)
            .add_system_to_stage(TiledRenderStage::Despawn, despawn_system);
    }
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

        info!("Num shapes in this tile: {}", tile.shapes.len());

        let mut set = std::collections::HashSet::<&Element>::with_capacity(tile.shapes.len());

        let mut num_duplicates = 0;

        // for idx in tile.shapes.iter() {
        //     let e = &(**flattened_elems)[*idx];
        //     if !set.insert(e) {
        //         num_duplicates += 1;
        //     }
        // }
        //
        // info!("Num duplicates: {num_duplicates}");

        let mut existing_shapes_iter = existing_lyon_shapes.iter_mut();
        for idx in tile.shapes.iter() {
            let el = &(**flattened_elems)[*idx];

            // let layer = lib_layers
            //     .get(el.layer)
            //     .expect("This Element's LayerKey does not exist in this Library's Layers")
            //     .layernum as u8;

            // let color = layers.get(&layer).unwrap();
            let color = *(Color::WHITE.set_a(ALPHA));

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

                let transform = Transform::from_translation(Vec3::new(0.0, 0.0, 0.0));

                let lyon_shape = GeometryBuilder::build_as(
                    &lyon_poly,
                    DrawMode::Outlined {
                        fill_mode: FillMode {
                            color,
                            options: FillOptions::default(),
                        },
                        outline_mode: StrokeMode {
                            color,
                            options: StrokeOptions::default().with_line_width(WIDTH),
                        },
                    },
                    transform,
                );

                let bundle = LyonShapeBundle {
                    lyon: lyon_shape,
                    ..default()
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
        if key.0 % 10 == 0 {
            rendering_complete_ev.send_default();
            continue;
        }

        let tile = tilemap.get(key).unwrap();

        let accumulation_handle = get_or_create_accumulation_texture(
            &mut commands,
            &mut accumulation_texture,
            &tilemap,
            &mut images,
        );

        let hires_handle = get_or_create_hires_texture(&mut hires_texture, &mut images);

        let tile_x = tile.extents.min().x - lower_left_res.x;
        let tile_y = tile.extents.min().y - lower_left_res.y;

        assert!(tile_x >= 0, "tile_x should be positive");
        assert!(tile_y >= 0, "tile_y should be positive");
        let x = tile_x / 4;
        let y = tile_y / 4;

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

        camera.projection.update(size.x as f32, size.y as f32);

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

        let physical_position = UVec2::new((x / 128) as u32, (y / 128) as u32);

        info!("viewport: {physical_position:?}");

        // The accumulation pass camera.
        commands
            .spawn_bundle(Camera2dBundle {
                camera: Camera {
                    priority: ACCUMULATION_CAMERA_PRIORITY,
                    target: RenderTarget::Image(accumulation_handle.clone()),
                    viewport: Some(Viewport {
                        // this is the same as the calculations we were doing to properly place the small texture's sprite
                        physical_position,
                        physical_size: UVec2::new(32, 32),
                        ..default()
                    }),
                    ..default()
                },
                camera_2d: Camera2d {
                    clear_color: ClearColorConfig::None,
                },
                ..default()
            })
            .insert(DOWNSCALING_PASS_LAYER)
            .insert(TextureCam);

        let s = rendering_done_channel.sender.clone();

        let key = key.clone();

        render_queue.on_submitted_work_done(move || {
            s.send(()).unwrap();
            info!("work done event sent for tile {key:?}!");
        });
    }
}

fn get_or_create_accumulation_texture(
    commands: &mut Commands,
    accumulation_texture: &mut Option<Handle<Image>>,
    tilemap: &TileMap,
    images: &mut Assets<Image>,
) -> Handle<Image> {
    if let Some(handle) = accumulation_texture.as_ref() {
        (*handle).clone()
    } else {
        let (grid_x, grid_y) = get_grid_shape(&tilemap);
        let size = Extent3d {
            width: grid_x * 32,
            height: grid_y * 32,
            ..default()
        };

        info!("accumulation texture size {size:?}");

        info!("CREATING NEW ACCUMULATION TEXTURE... SLEEPING FOR 5s");

        std::thread::sleep(Duration::from_secs(5));

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
                    custom_size: Some(Vec2::new(size.width as f32 * 1.1, size.height as f32 * 1.1)),
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

        handle
    }
}

fn get_or_create_hires_texture(
    hires_texture: &mut Option<Handle<Image>>,
    images: &mut Assets<Image>,
) -> Handle<Image> {
    if let Some(handle) = hires_texture.as_ref() {
        (*handle).clone()
    } else {
        let size = Extent3d {
            width: 4096,
            height: 4096,
            ..default()
        };

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

        image.resize(size);

        let handle = images.add(image);

        *hires_texture = Some(handle.clone());

        handle
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
            commands.entity(cam).despawn();
        }
        for mut vis in shape_q.iter_mut() {
            vis.is_visible = false;
        }
        rendering_complete_ev.send_default();
    }
}

#[derive(AsBindGroup, TypeUuid, Clone)]
#[uuid = "bc2f08eb-a0fb-43f1-a908-54871ea597d5"]
struct PostProcessingMaterial {
    /// This image will be the result of the main camera.
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
