use bevy::{
    core_pipeline::draw_3d_graph,
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    ecs::system::{
        lifetimeless::{Read, SQuery, SRes},
        SystemParamItem,
    },
    input::mouse::MouseMotion,
    pbr::SetShadowViewBindGroup,
    prelude::*,
    reflect::TypeUuid,
    render::{
        camera::{ActiveCamera, Camera3d},
        mesh::PrimitiveTopology,
        render_graph::{self, NodeRunError, RenderGraph, RenderGraphContext, SlotInfo, SlotType},
        render_phase::{
            AddRenderCommand, DrawFunctionId, DrawFunctions, EntityPhaseItem, EntityRenderCommand,
            PhaseItem, RenderCommand, RenderCommandResult, RenderPhase, TrackedRenderPass,
        },
        render_resource::{
            std140::AsStd140, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
            BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
            BufferBindingType, BufferInitDescriptor, BufferSize, BufferUsages, BufferVec,
            CachedRenderPipelineId, ColorTargetState, ColorWrites, CompareFunction, DepthBiasState,
            DepthStencilState, Face, FragmentState, FrontFace, IndexFormat, LoadOp,
            MultisampleState, Operations, PipelineCache, PolygonMode, PrimitiveState,
            RenderPassDepthStencilAttachment, RenderPassDescriptor, RenderPipelineDescriptor,
            ShaderStages, StencilFaceState, StencilState, TextureFormat, VertexState,
        },
        renderer::{RenderContext, RenderDevice, RenderQueue},
        texture::BevyDefault,
        view::{ExtractedView, ViewDepthTexture, ViewTarget, ViewUniform},
        RenderApp, RenderStage,
    },
};
use bytemuck::{cast_slice, Pod, Zeroable};
use rand::Rng;

fn main() {
    App::new()
        .insert_resource(WindowDescriptor {
            title: format!(
                "{} {} - quads",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ),
            width: 1280.0,
            height: 720.0,
            ..Default::default()
        })
        .add_plugins(DefaultPlugins)
        .add_plugin(FrameTimeDiagnosticsPlugin)
        .add_plugin(LogDiagnosticsPlugin::default())
        .add_plugin(QuadsPlugin)
        .add_startup_system(setup)
        .add_system(camera_controller)
        .run();
}

#[derive(Clone, Debug, Default)]
struct Quad {
    color: Color,
    center: Vec3,
    half_extents: Vec3,
}

impl Quad {
    pub fn random<R: Rng + ?Sized>(rng: &mut R, min: Vec3, max: Vec3) -> Self {
        Self {
            color: Color::WHITE,
            center: random_point_vec3(rng, min, max),
            half_extents: 0.01 * Vec3::ONE,
        }
    }
}

fn random_point_vec3<R: Rng + ?Sized>(rng: &mut R, min: Vec3, max: Vec3) -> Vec3 {
    Vec3::new(
        rng.gen_range(min.x..max.x),
        rng.gen_range(min.y..max.y),
        rng.gen_range(min.z..max.z),
    )
}

#[derive(Clone, Component, Debug, Default)]
struct Quads {
    data: Vec<Quad>,
    extracted: bool,
}

fn setup(mut commands: Commands) {
    commands
        .spawn_bundle(PerspectiveCameraBundle {
            transform: Transform::from_translation(50.0 * Vec3::Z).looking_at(Vec3::ZERO, Vec3::Y),
            ..default()
        })
        .insert(CameraController::default());

    let mut quads = Quads::default();
    let mut rng = rand::thread_rng();
    let min = -10.0 * Vec3::ONE;
    let max = 10.0 * Vec3::ONE;
    let n_quads = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(1_000_000);
    info!("Generating {} quads", n_quads);
    for _ in 0..n_quads {
        quads.data.push(Quad::random(&mut rng, min, max));
    }
    commands.spawn_bundle((quads,));
}

fn extract_quads_phase(mut commands: Commands, active_3d: Res<ActiveCamera<Camera3d>>) {
    if let Some(entity) = active_3d.get() {
        commands
            .get_or_spawn(entity)
            .insert(RenderPhase::<QuadsPhaseItem>::default());
    }
}

fn extract_quads(mut commands: Commands, mut quads: Query<(Entity, &mut Quads)>) {
    for (entity, mut quads) in quads.iter_mut() {
        if quads.extracted {
            commands.get_or_spawn(entity).insert(Quads {
                data: Vec::new(),
                extracted: true,
            });
        } else {
            commands.get_or_spawn(entity).insert(quads.clone());
            // NOTE: Set this after cloning so we don't extract next time
            quads.extracted = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
#[repr(C)]
struct GpuQuad {
    center: Vec4,
    half_extents: Vec4,
    color: [f32; 4],
}

impl From<&Quad> for GpuQuad {
    fn from(quad: &Quad) -> Self {
        Self {
            center: quad.center.extend(1.0),
            half_extents: quad.half_extents.extend(0.0),
            color: quad.color.as_rgba_f32(),
        }
    }
}

#[derive(Component)]
struct GpuQuads {
    index_buffer: Option<Buffer>,
    index_count: u32,
    instances: BufferVec<GpuQuad>,
}

impl Default for GpuQuads {
    fn default() -> Self {
        Self {
            index_buffer: None,
            index_count: 0,
            instances: BufferVec::<GpuQuad>::new(BufferUsages::STORAGE),
        }
    }
}

#[derive(Component)]
struct GpuQuadsBindGroup {
    bind_group: BindGroup,
}

fn prepare_quads(
    quads: Query<&Quads>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut gpu_quads: ResMut<GpuQuads>,
) {
    for quads in quads.iter() {
        if quads.extracted {
            continue;
        }
        for quad in quads.data.iter() {
            gpu_quads.instances.push(GpuQuad::from(quad));
        }
        gpu_quads.index_count = gpu_quads.instances.len() as u32 * 6;
        let mut indices = Vec::with_capacity(gpu_quads.index_count as usize);
        for i in 0..gpu_quads.instances.len() {
            let base = (i * 6) as u32;
            indices.push(base + 2);
            indices.push(base);
            indices.push(base + 1);
            indices.push(base + 1);
            indices.push(base + 3);
            indices.push(base + 2);
        }
        gpu_quads.index_buffer = Some(render_device.create_buffer_with_data(
            &BufferInitDescriptor {
                label: Some("gpu_quads_index_buffer"),
                contents: cast_slice(&indices),
                usage: BufferUsages::INDEX,
            },
        ));

        gpu_quads
            .instances
            .write_buffer(&*render_device, &*render_queue);
    }
}

pub struct QuadsPhaseItem {
    pub entity: Entity,
    pub draw_function: DrawFunctionId,
}

impl PhaseItem for QuadsPhaseItem {
    type SortKey = u32;

    #[inline]
    fn sort_key(&self) -> Self::SortKey {
        0
    }

    #[inline]
    fn draw_function(&self) -> DrawFunctionId {
        self.draw_function
    }
}

impl EntityPhaseItem for QuadsPhaseItem {
    #[inline]
    fn entity(&self) -> Entity {
        self.entity
    }
}

fn queue_quads(
    mut commands: Commands,
    opaque_3d_draw_functions: Res<DrawFunctions<QuadsPhaseItem>>,
    quads_pipeline: Res<QuadsPipeline>,
    render_device: Res<RenderDevice>,
    quads_query: Query<Entity, With<Quads>>,
    gpu_quads: Res<GpuQuads>,
    mut views: Query<&mut RenderPhase<QuadsPhaseItem>>,
) {
    let draw_quads = opaque_3d_draw_functions
        .read()
        .get_id::<DrawQuads>()
        .unwrap();

    for mut opaque_phase in views.iter_mut() {
        for entity in quads_query.iter() {
            commands
                .get_or_spawn(entity)
                .insert_bundle((GpuQuadsBindGroup {
                    bind_group: render_device.create_bind_group(&BindGroupDescriptor {
                        label: Some("gpu_quads_bind_group"),
                        layout: &quads_pipeline.quads_layout,
                        entries: &[BindGroupEntry {
                            binding: 0,
                            resource: gpu_quads.instances.buffer().unwrap().as_entire_binding(),
                        }],
                    }),
                },));
            opaque_phase.add(QuadsPhaseItem {
                entity,
                draw_function: draw_quads,
            });
        }
    }
}

mod node {
    pub const QUADS_PASS: &str = "quads_pass";
}

pub struct QuadsPassNode {
    query: QueryState<
        (
            &'static RenderPhase<QuadsPhaseItem>,
            &'static ViewTarget,
            &'static ViewDepthTexture,
        ),
        With<ExtractedView>,
    >,
}

impl QuadsPassNode {
    pub const IN_VIEW: &'static str = "view";

    pub fn new(world: &mut World) -> Self {
        Self {
            query: QueryState::new(world),
        }
    }
}

impl render_graph::Node for QuadsPassNode {
    fn input(&self) -> Vec<SlotInfo> {
        vec![SlotInfo::new(QuadsPassNode::IN_VIEW, SlotType::Entity)]
    }

    fn update(&mut self, world: &mut World) {
        self.query.update_archetypes(world);
    }

    fn run(
        &self,
        graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let view_entity = graph.get_input_entity(Self::IN_VIEW)?;
        let (quads_phase, target, depth) = match self.query.get_manual(world, view_entity) {
            Ok(query) => query,
            Err(_) => return Ok(()), // No window
        };

        #[cfg(feature = "trace")]
        let _main_quads_pass_span = info_span!("main_quads_pass").entered();
        let pass_descriptor = RenderPassDescriptor {
            label: Some("main_quads_pass"),
            // NOTE: The quads pass loads the color
            // buffer as well as writing to it.
            color_attachments: &[target.get_color_attachment(Operations {
                load: LoadOp::Load,
                store: true,
            })],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: &depth.view,
                // NOTE: The quads main pass loads the depth buffer and possibly overwrites it
                depth_ops: Some(Operations {
                    load: LoadOp::Load,
                    store: true,
                }),
                stencil_ops: None,
            }),
        };

        let draw_functions = world.resource::<DrawFunctions<QuadsPhaseItem>>();

        let render_pass = render_context
            .command_encoder
            .begin_render_pass(&pass_descriptor);
        let mut draw_functions = draw_functions.write();
        let mut tracked_pass = TrackedRenderPass::new(render_pass);
        for item in &quads_phase.items {
            let draw_function = draw_functions.get_mut(item.draw_function).unwrap();
            draw_function.draw(world, &mut tracked_pass, view_entity, item);
        }

        Ok(())
    }
}

struct QuadsPlugin;

impl Plugin for QuadsPlugin {
    fn build(&self, app: &mut App) {
        app.world.resource_mut::<Assets<Shader>>().set_untracked(
            QUADS_SHADER_HANDLE,
            Shader::from_wgsl(include_str!("quads.wgsl")),
        );

        let render_app = app.sub_app_mut(RenderApp);

        render_app
            .init_resource::<DrawFunctions<QuadsPhaseItem>>()
            .add_render_command::<QuadsPhaseItem, DrawQuads>()
            .init_resource::<QuadsPipeline>()
            .init_resource::<GpuQuads>()
            .add_system_to_stage(RenderStage::Extract, extract_quads_phase)
            .add_system_to_stage(RenderStage::Extract, extract_quads)
            .add_system_to_stage(RenderStage::Prepare, prepare_quads)
            .add_system_to_stage(RenderStage::Queue, queue_quads);

        let quads_pass_node = QuadsPassNode::new(&mut render_app.world);
        let mut graph = render_app.world.resource_mut::<RenderGraph>();
        let draw_3d_graph = graph.get_sub_graph_mut(draw_3d_graph::NAME).unwrap();
        draw_3d_graph.add_node(node::QUADS_PASS, quads_pass_node);
        draw_3d_graph
            .add_node_edge(node::QUADS_PASS, draw_3d_graph::node::MAIN_PASS)
            .unwrap();
        draw_3d_graph
            .add_slot_edge(
                draw_3d_graph.input_node().unwrap().id,
                draw_3d_graph::input::VIEW_ENTITY,
                node::QUADS_PASS,
                QuadsPassNode::IN_VIEW,
            )
            .unwrap();
    }
}

struct QuadsPipeline {
    pipeline_id: CachedRenderPipelineId,
    quads_layout: BindGroupLayout,
}

const QUADS_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 7659167879172469997);

impl FromWorld for QuadsPipeline {
    fn from_world(world: &mut World) -> Self {
        let view_layout =
            world
                .resource::<RenderDevice>()
                .create_bind_group_layout(&BindGroupLayoutDescriptor {
                    entries: &[
                        // View
                        BindGroupLayoutEntry {
                            binding: 0,
                            visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                            ty: BindingType::Buffer {
                                ty: BufferBindingType::Uniform,
                                has_dynamic_offset: true,
                                min_binding_size: BufferSize::new(
                                    ViewUniform::std140_size_static() as u64,
                                ),
                            },
                            count: None,
                        },
                    ],
                    label: Some("shadow_view_layout"),
                });

        let quads_layout =
            world
                .resource::<RenderDevice>()
                .create_bind_group_layout(&BindGroupLayoutDescriptor {
                    label: None,
                    entries: &[BindGroupLayoutEntry {
                        binding: 0,
                        visibility: ShaderStages::VERTEX,
                        ty: BindingType::Buffer {
                            ty: BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: BufferSize::new(0),
                        },
                        count: None,
                    }],
                });

        let mut pipeline_cache = world.resource_mut::<PipelineCache>();
        let pipeline_id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("quads_pipeline".into()),
            layout: Some(vec![view_layout, quads_layout.clone()]),
            vertex: VertexState {
                shader: QUADS_SHADER_HANDLE.typed(),
                shader_defs: vec![],
                entry_point: "vertex".into(),
                buffers: vec![],
            },
            fragment: Some(FragmentState {
                shader: QUADS_SHADER_HANDLE.typed(),
                shader_defs: vec![],
                entry_point: "fragment".into(),
                targets: vec![ColorTargetState {
                    format: TextureFormat::bevy_default(),
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                }],
            }),
            primitive: PrimitiveState {
                front_face: FrontFace::Ccw,
                cull_mode: Some(Face::Back),
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
                topology: PrimitiveTopology::TriangleList,
                strip_index_format: None,
            },
            depth_stencil: Some(DepthStencilState {
                format: TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: CompareFunction::Greater,
                stencil: StencilState {
                    front: StencilFaceState::IGNORE,
                    back: StencilFaceState::IGNORE,
                    read_mask: 0,
                    write_mask: 0,
                },
                bias: DepthBiasState {
                    constant: 0,
                    slope_scale: 0.0,
                    clamp: 0.0,
                },
            }),
            multisample: MultisampleState {
                count: Msaa::default().samples,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
        });

        Self {
            pipeline_id,
            quads_layout,
        }
    }
}

type DrawQuads = (
    SetQuadsPipeline,
    SetShadowViewBindGroup<0>,
    SetGpuQuadsBindGroup<1>,
    DrawVertexPulledQuads,
);

struct SetQuadsPipeline;
impl<P: PhaseItem> RenderCommand<P> for SetQuadsPipeline {
    type Param = (SRes<PipelineCache>, SRes<QuadsPipeline>);
    #[inline]
    fn render<'w>(
        _view: Entity,
        _item: &P,
        params: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let (pipeline_cache, quads_pipeline) = params;
        if let Some(pipeline) = pipeline_cache
            .into_inner()
            .get_render_pipeline(quads_pipeline.pipeline_id)
        {
            pass.set_render_pipeline(pipeline);
            RenderCommandResult::Success
        } else {
            RenderCommandResult::Failure
        }
    }
}

struct SetGpuQuadsBindGroup<const I: usize>;
impl<const I: usize> EntityRenderCommand for SetGpuQuadsBindGroup<I> {
    type Param = SQuery<Read<GpuQuadsBindGroup>>;

    #[inline]
    fn render<'w>(
        _view: Entity,
        item: Entity,
        gpu_quads_bind_groups: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let gpu_quads_bind_group = gpu_quads_bind_groups.get_inner(item).unwrap();
        pass.set_bind_group(I, &gpu_quads_bind_group.bind_group, &[]);

        RenderCommandResult::Success
    }
}

struct DrawVertexPulledQuads;
impl EntityRenderCommand for DrawVertexPulledQuads {
    type Param = SRes<GpuQuads>;

    #[inline]
    fn render<'w>(
        _view: Entity,
        _item: Entity,
        gpu_quads: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let gpu_quads = gpu_quads.into_inner();
        pass.set_index_buffer(
            gpu_quads.index_buffer.as_ref().unwrap().slice(..),
            0,
            IndexFormat::Uint32,
        );
        pass.draw_indexed(0..gpu_quads.index_count, 0, 0..1);
        RenderCommandResult::Success
    }
}

#[derive(Component)]
struct CameraController {
    pub enabled: bool,
    pub initialized: bool,
    pub sensitivity: f32,
    pub key_forward: KeyCode,
    pub key_back: KeyCode,
    pub key_left: KeyCode,
    pub key_right: KeyCode,
    pub key_up: KeyCode,
    pub key_down: KeyCode,
    pub key_run: KeyCode,
    pub key_enable_mouse: MouseButton,
    pub walk_speed: f32,
    pub run_speed: f32,
    pub friction: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub velocity: Vec3,
}

impl Default for CameraController {
    fn default() -> Self {
        Self {
            enabled: true,
            initialized: false,
            sensitivity: 0.5,
            key_forward: KeyCode::W,
            key_back: KeyCode::S,
            key_left: KeyCode::A,
            key_right: KeyCode::D,
            key_up: KeyCode::E,
            key_down: KeyCode::Q,
            key_run: KeyCode::LShift,
            key_enable_mouse: MouseButton::Left,
            walk_speed: 2.0,
            run_speed: 6.0,
            friction: 0.5,
            pitch: 0.0,
            yaw: 0.0,
            velocity: Vec3::ZERO,
        }
    }
}

fn camera_controller(
    time: Res<Time>,
    mut mouse_events: EventReader<MouseMotion>,
    mouse_button_input: Res<Input<MouseButton>>,
    key_input: Res<Input<KeyCode>>,
    mut query: Query<(&mut Transform, &mut CameraController), With<Camera>>,
) {
    let dt = time.delta_seconds();

    if let Ok((mut transform, mut options)) = query.get_single_mut() {
        if !options.initialized {
            let (yaw, pitch, _roll) = transform.rotation.to_euler(EulerRot::YXZ);
            options.yaw = yaw;
            options.pitch = pitch;
            options.initialized = true;
        }
        if !options.enabled {
            return;
        }

        // Handle key input
        let mut axis_input = Vec3::ZERO;
        if key_input.pressed(options.key_forward) {
            axis_input.z += 1.0;
        }
        if key_input.pressed(options.key_back) {
            axis_input.z -= 1.0;
        }
        if key_input.pressed(options.key_right) {
            axis_input.x += 1.0;
        }
        if key_input.pressed(options.key_left) {
            axis_input.x -= 1.0;
        }
        if key_input.pressed(options.key_up) {
            axis_input.y += 1.0;
        }
        if key_input.pressed(options.key_down) {
            axis_input.y -= 1.0;
        }

        // Apply movement update
        if axis_input != Vec3::ZERO {
            let max_speed = if key_input.pressed(options.key_run) {
                options.run_speed
            } else {
                options.walk_speed
            };
            options.velocity = axis_input.normalize() * max_speed;
        } else {
            let friction = options.friction.clamp(0.0, 1.0);
            options.velocity *= 1.0 - friction;
            if options.velocity.length_squared() < 1e-6 {
                options.velocity = Vec3::ZERO;
            }
        }
        let forward = transform.forward();
        let right = transform.right();
        transform.translation += options.velocity.x * dt * right
            + options.velocity.y * dt * Vec3::Y
            + options.velocity.z * dt * forward;

        // Handle mouse input
        let mut mouse_delta = Vec2::ZERO;
        if mouse_button_input.pressed(options.key_enable_mouse) {
            for mouse_event in mouse_events.iter() {
                mouse_delta += mouse_event.delta;
            }
        }

        if mouse_delta != Vec2::ZERO {
            // Apply look update
            let (pitch, yaw) = (
                (options.pitch - mouse_delta.y * 0.5 * options.sensitivity * dt).clamp(
                    -0.99 * std::f32::consts::FRAC_PI_2,
                    0.99 * std::f32::consts::FRAC_PI_2,
                ),
                options.yaw - mouse_delta.x * options.sensitivity * dt,
            );
            transform.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, 0.0);
            options.pitch = pitch;
            options.yaw = yaw;
        }
    }
}
