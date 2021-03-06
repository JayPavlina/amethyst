use crate::{
    batch::{GroupIterator, OrderedTwoLevelBatch, TwoLevelBatch},
    mtl::{FullTextureSet, Material, StaticTextureSet},
    pipeline::{PipelineDescBuilder, PipelinesBuilder},
    pod::{SkinnedVertexArgs, VertexArgs},
    resources::Tint,
    skinning::JointTransforms,
    submodules::{DynamicVertex, EnvironmentSub, MaterialId, MaterialSub, SkinningSub},
    transparent::Transparent,
    types::{Backend, Mesh},
    util,
    visibility::Visibility,
};
use amethyst_assets::{AssetStorage, Handle};
use amethyst_core::{
    ecs::{Join, Read, ReadExpect, ReadStorage, Resources, SystemData},
    transform::Transform,
    Hidden, HiddenPropagate,
};
use derivative::Derivative;
use rendy::{
    command::{QueueId, RenderPassEncoder},
    factory::Factory,
    graph::{
        render::{PrepareResult, RenderGroup, RenderGroupDesc},
        GraphContext, NodeBuffer, NodeImage,
    },
    hal::{self, device::Device, pso},
    mesh::{AsVertex, VertexFormat},
    shader::{Shader, SpirvShader},
};
use smallvec::SmallVec;
use std::marker::PhantomData;

macro_rules! profile_scope_impl {
    ($string:expr) => {
        #[cfg(feature = "profiler")]
        let _profile_scope = thread_profiler::ProfileScope::new(format!(
            "{} {}: {}",
            module_path!(),
            <T as Base3DPassDef<B>>::NAME,
            $string
        ));
    };
}

pub trait Base3DPassDef<B: Backend>: 'static + std::fmt::Debug + Send + Sync {
    const NAME: &'static str;
    type TextureSet: for<'a> StaticTextureSet<'a>;
    fn vertex_shader() -> &'static SpirvShader;
    fn vertex_skinned_shader() -> &'static SpirvShader;
    fn fragment_shader() -> &'static SpirvShader;
    fn base_format() -> Vec<VertexFormat>;
    fn skinned_format() -> Vec<VertexFormat>;
}

/// Draw opaque 3d mesh with specified shaders and texture set
#[derive(Clone, Derivative)]
#[derivative(Debug(bound = ""), Default(bound = ""))]
pub struct DrawBase3DDesc<B: Backend, T: Base3DPassDef<B>> {
    skinning: bool,
    marker: PhantomData<(B, T)>,
}

impl<B: Backend, T: Base3DPassDef<B>> DrawBase3DDesc<B, T> {
    /// Create pass in default configuration
    pub fn new() -> Self {
        Self {
            skinning: false,
            marker: PhantomData,
        }
    }

    /// Create pass in with vertex skinning enabled
    pub fn skinned() -> Self {
        Self {
            skinning: true,
            marker: PhantomData,
        }
    }
}

impl<B: Backend, T: Base3DPassDef<B>> RenderGroupDesc<B, Resources> for DrawBase3DDesc<B, T> {
    fn build(
        self,
        _ctx: &GraphContext<B>,
        factory: &mut Factory<B>,
        _queue: QueueId,
        _aux: &Resources,
        framebuffer_width: u32,
        framebuffer_height: u32,
        subpass: hal::pass::Subpass<'_, B>,
        _buffers: Vec<NodeBuffer>,
        _images: Vec<NodeImage>,
    ) -> Result<Box<dyn RenderGroup<B, Resources>>, failure::Error> {
        profile_scope_impl!("build");

        let env = EnvironmentSub::new(factory)?;
        let materials = MaterialSub::new(factory)?;
        let skinning = SkinningSub::new(factory)?;

        let mut vertex_format_base = T::base_format();
        let mut vertex_format_skinned = T::skinned_format();

        let (mut pipelines, pipeline_layout) = build_pipelines::<B, T>(
            factory,
            subpass,
            framebuffer_width,
            framebuffer_height,
            &vertex_format_base,
            &vertex_format_skinned,
            self.skinning,
            false,
            vec![
                env.raw_layout(),
                materials.raw_layout(),
                skinning.raw_layout(),
            ],
        )?;

        vertex_format_base.sort();
        vertex_format_skinned.sort();

        Ok(Box::new(DrawBase3D::<B, T> {
            pipeline_basic: pipelines.remove(0),
            pipeline_skinned: pipelines.pop(),
            pipeline_layout,
            static_batches: Default::default(),
            skinned_batches: Default::default(),
            vertex_format_base,
            vertex_format_skinned,
            env,
            materials,
            skinning,
            models: DynamicVertex::new(),
            skinned_models: DynamicVertex::new(),
            marker: PhantomData,
        }))
    }
}

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub struct DrawBase3D<B: Backend, T: Base3DPassDef<B>> {
    pipeline_basic: B::GraphicsPipeline,
    pipeline_skinned: Option<B::GraphicsPipeline>,
    pipeline_layout: B::PipelineLayout,
    static_batches: TwoLevelBatch<MaterialId, u32, SmallVec<[VertexArgs; 4]>>,
    skinned_batches: TwoLevelBatch<MaterialId, u32, SmallVec<[SkinnedVertexArgs; 4]>>,
    vertex_format_base: Vec<VertexFormat>,
    vertex_format_skinned: Vec<VertexFormat>,
    env: EnvironmentSub<B>,
    materials: MaterialSub<B, T::TextureSet>,
    skinning: SkinningSub<B>,
    models: DynamicVertex<B, VertexArgs>,
    skinned_models: DynamicVertex<B, SkinnedVertexArgs>,
    marker: PhantomData<T>,
}

impl<B: Backend, T: Base3DPassDef<B>> RenderGroup<B, Resources> for DrawBase3D<B, T> {
    fn prepare(
        &mut self,
        factory: &Factory<B>,
        _queue: QueueId,
        index: usize,
        _subpass: hal::pass::Subpass<'_, B>,
        resources: &Resources,
    ) -> PrepareResult {
        profile_scope_impl!("prepare");

        let (
            mesh_storage,
            visibility,
            transparent,
            hiddens,
            hiddens_prop,
            meshes,
            materials,
            transforms,
            joints,
            tints,
        ) = <(
            Read<AssetStorage<Mesh>>,
            Option<Read<Visibility>>,
            ReadStorage<Transparent>,
            ReadStorage<Hidden>,
            ReadStorage<HiddenPropagate>,
            ReadStorage<Handle<Mesh>>,
            ReadStorage<Handle<Material>>,
            ReadStorage<Transform>,
            ReadStorage<JointTransforms>,
            ReadStorage<Tint>,
        )>::fetch(resources);

        // Prepare environment
        self.env.process(factory, index, resources);
        self.materials.maintain();

        self.static_batches.clear_inner();
        self.skinned_batches.clear_inner();

        let materials_ref = &mut self.materials;
        let skinning_ref = &mut self.skinning;
        let statics_ref = &mut self.static_batches;
        let skinned_ref = &mut self.skinned_batches;

        let static_input = || ((&materials, &meshes, &transforms, tints.maybe()), !&joints);

        let skinned_input = || (&materials, &meshes, &transforms, tints.maybe(), &joints);

        match &visibility {
            None => {
                profile_scope_impl!("gather_novisibility");

                (static_input(), (!&hiddens, !&hiddens_prop, !&transparent))
                    .join()
                    .map(|(((mat, mesh, tform, tint), _), _)| {
                        ((mat, mesh.id()), VertexArgs::from_object_data(tform, tint))
                    })
                    .for_each_group(|(mat, mesh_id), data| {
                        if mesh_storage.contains_id(mesh_id) {
                            if let Some((mat, _)) = materials_ref.insert(factory, resources, mat) {
                                statics_ref.insert(mat, mesh_id, data.drain(..));
                            }
                        }
                    });

                if self.pipeline_skinned.is_some() {
                    profile_scope_impl!("gather_novisibility_skinning");

                    (skinned_input(), (!&hiddens, !&hiddens_prop))
                        .join()
                        .map(|((mat, mesh, tform, tint, joints), _)| {
                            (
                                (mat, mesh.id()),
                                SkinnedVertexArgs::from_object_data(
                                    tform,
                                    tint,
                                    skinning_ref.insert(joints),
                                ),
                            )
                        })
                        .for_each_group(|(mat, mesh_id), data| {
                            if mesh_storage.contains_id(mesh_id) {
                                if let Some((mat, _)) =
                                    materials_ref.insert(factory, resources, mat)
                                {
                                    skinned_ref.insert(mat, mesh_id, data.drain(..));
                                }
                            }
                        });
                }
            }
            Some(visibility) => {
                profile_scope_impl!("prepare_visibility");

                (static_input(), &visibility.visible_unordered)
                    .join()
                    .map(|(((mat, mesh, tform, tint), _), _)| {
                        ((mat, mesh.id()), VertexArgs::from_object_data(tform, tint))
                    })
                    .for_each_group(|(mat, mesh_id), data| {
                        if mesh_storage.contains_id(mesh_id) {
                            if let Some((mat, _)) = materials_ref.insert(factory, resources, mat) {
                                statics_ref.insert(mat, mesh_id, data.drain(..));
                            }
                        }
                    });

                if self.pipeline_skinned.is_some() {
                    profile_scope_impl!("prepare_visibility_skinning");

                    (skinned_input(), &visibility.visible_unordered)
                        .join()
                        .map(|((mat, mesh, tform, tint, joints), _)| {
                            (
                                (mat, mesh.id()),
                                SkinnedVertexArgs::from_object_data(
                                    tform,
                                    tint,
                                    skinning_ref.insert(joints),
                                ),
                            )
                        })
                        .for_each_group(|(mat, mesh_id), data| {
                            if mesh_storage.contains_id(mesh_id) {
                                if let Some((mat, _)) =
                                    materials_ref.insert(factory, resources, mat)
                                {
                                    skinned_ref.insert(mat, mesh_id, data.drain(..));
                                }
                            }
                        });
                }
            }
        };

        {
            profile_scope_impl!("write");

            self.static_batches.prune();
            self.skinned_batches.prune();

            self.models.write(
                factory,
                index,
                self.static_batches.count() as u64,
                self.static_batches.data(),
            );

            self.skinned_models.write(
                factory,
                index,
                self.skinned_batches.count() as u64,
                self.skinned_batches.data(),
            );
            self.skinning.commit(factory, index);
        }
        PrepareResult::DrawRecord
    }

    fn draw_inline(
        &mut self,
        mut encoder: RenderPassEncoder<'_, B>,
        index: usize,
        _subpass: hal::pass::Subpass<'_, B>,
        resources: &Resources,
    ) {
        profile_scope_impl!("draw");

        let mesh_storage = <Read<'_, AssetStorage<Mesh>>>::fetch(resources);
        let models_loc = self.vertex_format_base.len() as u32;
        let skin_models_loc = self.vertex_format_skinned.len() as u32;

        encoder.bind_graphics_pipeline(&self.pipeline_basic);
        self.env.bind(index, &self.pipeline_layout, 0, &mut encoder);

        if self.models.bind(index, models_loc, &mut encoder) {
            let mut instances_drawn = 0;
            for (&mat_id, batches) in self.static_batches.iter() {
                if self.materials.loaded(mat_id) {
                    self.materials
                        .bind(&self.pipeline_layout, 1, mat_id, &mut encoder);
                    for (mesh_id, batch_data) in batches {
                        debug_assert!(mesh_storage.contains_id(*mesh_id));
                        if let Some(mesh) =
                            B::unwrap_mesh(unsafe { mesh_storage.get_by_id_unchecked(*mesh_id) })
                        {
                            mesh.bind_and_draw(
                                0,
                                &self.vertex_format_base,
                                instances_drawn..instances_drawn + batch_data.len() as u32,
                                &mut encoder,
                            )
                            .unwrap();
                        }
                        instances_drawn += batch_data.len() as u32;
                    }
                }
            }
        }

        if let Some(pipeline_skinned) = self.pipeline_skinned.as_ref() {
            encoder.bind_graphics_pipeline(pipeline_skinned);

            if self
                .skinned_models
                .bind(index, skin_models_loc, &mut encoder)
            {
                self.skinning
                    .bind(index, &self.pipeline_layout, 2, &mut encoder);

                let mut instances_drawn = 0;
                for (&mat_id, batches) in self.skinned_batches.iter() {
                    if self.materials.loaded(mat_id) {
                        self.materials
                            .bind(&self.pipeline_layout, 1, mat_id, &mut encoder);
                        for (mesh_id, batch_data) in batches {
                            debug_assert!(mesh_storage.contains_id(*mesh_id));
                            if let Some(mesh) = B::unwrap_mesh(unsafe {
                                mesh_storage.get_by_id_unchecked(*mesh_id)
                            }) {
                                mesh.bind_and_draw(
                                    0,
                                    &self.vertex_format_skinned,
                                    instances_drawn..instances_drawn + batch_data.len() as u32,
                                    &mut encoder,
                                )
                                .unwrap();
                            }
                            instances_drawn += batch_data.len() as u32;
                        }
                    }
                }
            }
        }
    }

    fn dispose(mut self: Box<Self>, factory: &mut Factory<B>, _aux: &Resources) {
        profile_scope_impl!("dispose");
        unsafe {
            factory
                .device()
                .destroy_graphics_pipeline(self.pipeline_basic);
            self.pipeline_skinned.take().map(|pipeline| {
                factory.device().destroy_graphics_pipeline(pipeline);
            });
            factory
                .device()
                .destroy_pipeline_layout(self.pipeline_layout);
        }
    }
}

/// Draw transparent mesh with physically based lighting
#[derive(Clone, Derivative)]
#[derivative(Debug(bound = ""), Default(bound = ""))]
pub struct DrawBase3DTransparentDesc<B: Backend, T: Base3DPassDef<B>> {
    skinning: bool,
    marker: PhantomData<(B, T)>,
}

impl<B: Backend, T: Base3DPassDef<B>> DrawBase3DTransparentDesc<B, T> {
    /// Create pass in default configuration
    pub fn new() -> Self {
        Self {
            skinning: false,
            marker: PhantomData,
        }
    }

    /// Create pass in with vertex skinning enabled
    pub fn skinned() -> Self {
        Self {
            skinning: true,
            marker: PhantomData,
        }
    }
}

impl<B: Backend, T: Base3DPassDef<B>> RenderGroupDesc<B, Resources>
    for DrawBase3DTransparentDesc<B, T>
{
    fn build(
        self,
        _ctx: &GraphContext<B>,
        factory: &mut Factory<B>,
        _queue: QueueId,
        _aux: &Resources,
        framebuffer_width: u32,
        framebuffer_height: u32,
        subpass: hal::pass::Subpass<'_, B>,
        _buffers: Vec<NodeBuffer>,
        _images: Vec<NodeImage>,
    ) -> Result<Box<dyn RenderGroup<B, Resources>>, failure::Error> {
        let env = EnvironmentSub::new(factory)?;
        let materials = MaterialSub::new(factory)?;
        let skinning = SkinningSub::new(factory)?;

        let mut vertex_format_base = T::base_format();
        let mut vertex_format_skinned = T::skinned_format();

        let (mut pipelines, pipeline_layout) = build_pipelines::<B, T>(
            factory,
            subpass,
            framebuffer_width,
            framebuffer_height,
            &vertex_format_base,
            &vertex_format_skinned,
            self.skinning,
            true,
            vec![
                env.raw_layout(),
                materials.raw_layout(),
                skinning.raw_layout(),
            ],
        )?;

        vertex_format_base.sort();
        vertex_format_skinned.sort();

        Ok(Box::new(DrawBase3DTransparent::<B, T> {
            pipeline_basic: pipelines.remove(0),
            pipeline_skinned: pipelines.pop(),
            pipeline_layout,
            static_batches: Default::default(),
            skinned_batches: Default::default(),
            vertex_format_base,
            vertex_format_skinned,
            env,
            materials,
            skinning,
            models: DynamicVertex::new(),
            skinned_models: DynamicVertex::new(),
            change: Default::default(),
            marker: PhantomData,
        }))
    }
}

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub struct DrawBase3DTransparent<B: Backend, T: Base3DPassDef<B>> {
    pipeline_basic: B::GraphicsPipeline,
    pipeline_skinned: Option<B::GraphicsPipeline>,
    pipeline_layout: B::PipelineLayout,
    static_batches: OrderedTwoLevelBatch<MaterialId, u32, VertexArgs>,
    skinned_batches: OrderedTwoLevelBatch<MaterialId, u32, SkinnedVertexArgs>,
    vertex_format_base: Vec<VertexFormat>,
    vertex_format_skinned: Vec<VertexFormat>,
    env: EnvironmentSub<B>,
    materials: MaterialSub<B, FullTextureSet>,
    skinning: SkinningSub<B>,
    models: DynamicVertex<B, VertexArgs>,
    skinned_models: DynamicVertex<B, SkinnedVertexArgs>,
    change: util::ChangeDetection,
    marker: PhantomData<(T)>,
}

impl<B: Backend, T: Base3DPassDef<B>> RenderGroup<B, Resources> for DrawBase3DTransparent<B, T> {
    fn prepare(
        &mut self,
        factory: &Factory<B>,
        _queue: QueueId,
        index: usize,
        _subpass: hal::pass::Subpass<'_, B>,
        resources: &Resources,
    ) -> PrepareResult {
        let (mesh_storage, visibility, meshes, materials, transforms, joints, tints) =
            <(
                Read<AssetStorage<Mesh>>,
                ReadExpect<Visibility>,
                ReadStorage<Handle<Mesh>>,
                ReadStorage<Handle<Material>>,
                ReadStorage<Transform>,
                ReadStorage<JointTransforms>,
                ReadStorage<Tint>,
            )>::fetch(resources);

        // Prepare environment
        self.env.process(factory, index, resources);
        self.materials.maintain();

        self.static_batches.swap_clear();
        self.skinned_batches.swap_clear();

        let materials_ref = &mut self.materials;
        let skinning_ref = &mut self.skinning;
        let statics_ref = &mut self.static_batches;
        let skinned_ref = &mut self.skinned_batches;
        let mut changed = false;

        let mut joined = ((&materials, &meshes, &transforms, tints.maybe()), !&joints).join();
        visibility
            .visible_ordered
            .iter()
            .filter_map(|e| joined.get_unchecked(e.id()))
            .map(|((mat, mesh, tform, tint), _)| {
                ((mat, mesh.id()), VertexArgs::from_object_data(tform, tint))
            })
            .for_each_group(|(mat, mesh_id), data| {
                if mesh_storage.contains_id(mesh_id) {
                    if let Some((mat, this_changed)) = materials_ref.insert(factory, resources, mat)
                    {
                        changed = changed || this_changed;
                        statics_ref.insert(mat, mesh_id, data.drain(..));
                    }
                }
            });

        if self.pipeline_skinned.is_some() {
            let mut joined = (&materials, &meshes, &transforms, tints.maybe(), &joints).join();

            visibility
                .visible_ordered
                .iter()
                .filter_map(|e| joined.get_unchecked(e.id()))
                .map(|(mat, mesh, tform, tint, joints)| {
                    (
                        (mat, mesh.id()),
                        SkinnedVertexArgs::from_object_data(
                            tform,
                            tint,
                            skinning_ref.insert(joints),
                        ),
                    )
                })
                .for_each_group(|(mat, mesh_id), data| {
                    if mesh_storage.contains_id(mesh_id) {
                        if let Some((mat, this_changed)) =
                            materials_ref.insert(factory, resources, mat)
                        {
                            changed = changed || this_changed;
                            skinned_ref.insert(mat, mesh_id, data.drain(..));
                        }
                    }
                });
        }

        self.models.write(
            factory,
            index,
            self.static_batches.count() as u64,
            Some(self.static_batches.data()),
        );

        self.skinned_models.write(
            factory,
            index,
            self.skinned_batches.count() as u64,
            Some(self.skinned_batches.data()),
        );

        self.skinning.commit(factory, index);

        changed = changed || self.static_batches.changed();
        changed = changed || self.skinned_batches.changed();

        self.change.prepare_result(index, changed)
    }

    fn draw_inline(
        &mut self,
        mut encoder: RenderPassEncoder<'_, B>,
        index: usize,
        _subpass: hal::pass::Subpass<'_, B>,
        resources: &Resources,
    ) {
        let mesh_storage = <Read<'_, AssetStorage<Mesh>>>::fetch(resources);
        let layout = &self.pipeline_layout;
        let encoder = &mut encoder;

        let models_loc = self.vertex_format_base.len() as u32;
        let skin_models_loc = self.vertex_format_skinned.len() as u32;

        encoder.bind_graphics_pipeline(&self.pipeline_basic);
        self.env.bind(index, layout, 0, encoder);

        if self.models.bind(index, models_loc, encoder) {
            for (&mat, batches) in self.static_batches.iter() {
                if self.materials.loaded(mat) {
                    self.materials.bind(layout, 1, mat, encoder);
                    for (mesh, range) in batches {
                        debug_assert!(mesh_storage.contains_id(*mesh));
                        if let Some(mesh) =
                            B::unwrap_mesh(unsafe { mesh_storage.get_by_id_unchecked(*mesh) })
                        {
                            mesh.bind_and_draw(0, &self.vertex_format_base, range.clone(), encoder)
                                .unwrap();
                        }
                    }
                }
            }
        }

        if let Some(pipeline_skinned) = self.pipeline_skinned.as_ref() {
            encoder.bind_graphics_pipeline(pipeline_skinned);

            if self.skinned_models.bind(index, skin_models_loc, encoder) {
                self.skinning.bind(index, layout, 2, encoder);
                for (&mat, batches) in self.skinned_batches.iter() {
                    if self.materials.loaded(mat) {
                        self.materials.bind(layout, 1, mat, encoder);
                        for (mesh, range) in batches {
                            debug_assert!(mesh_storage.contains_id(*mesh));
                            if let Some(mesh) =
                                B::unwrap_mesh(unsafe { mesh_storage.get_by_id_unchecked(*mesh) })
                            {
                                mesh.bind_and_draw(
                                    0,
                                    &self.vertex_format_skinned,
                                    range.clone(),
                                    encoder,
                                )
                                .unwrap();
                            }
                        }
                    }
                }
            }
        }
    }

    fn dispose(mut self: Box<Self>, factory: &mut Factory<B>, _aux: &Resources) {
        unsafe {
            factory
                .device()
                .destroy_graphics_pipeline(self.pipeline_basic);
            self.pipeline_skinned.take().map(|pipeline| {
                factory.device().destroy_graphics_pipeline(pipeline);
            });
            factory
                .device()
                .destroy_pipeline_layout(self.pipeline_layout);
        }
    }
}

fn build_pipelines<B: Backend, T: Base3DPassDef<B>>(
    factory: &Factory<B>,
    subpass: hal::pass::Subpass<'_, B>,
    framebuffer_width: u32,
    framebuffer_height: u32,
    vertex_format_base: &[VertexFormat],
    vertex_format_skinned: &[VertexFormat],
    skinning: bool,
    transparent: bool,
    layouts: Vec<&B::DescriptorSetLayout>,
) -> Result<(Vec<B::GraphicsPipeline>, B::PipelineLayout), failure::Error> {
    let pipeline_layout = unsafe {
        factory
            .device()
            .create_pipeline_layout(layouts, None as Option<(_, _)>)
    }?;

    let vertex_desc = vertex_format_base
        .iter()
        .map(|f| (f.clone(), pso::VertexInputRate::Vertex))
        .chain(Some((
            VertexArgs::vertex(),
            pso::VertexInputRate::Instance(1),
        )))
        .collect::<Vec<_>>();

    let shader_vertex_basic = unsafe { T::vertex_shader().module(factory).unwrap() };
    let shader_fragment = unsafe { T::fragment_shader().module(factory).unwrap() };
    let pipe_desc = PipelineDescBuilder::new()
        .with_vertex_desc(&vertex_desc)
        .with_shaders(util::simple_shader_set(
            &shader_vertex_basic,
            Some(&shader_fragment),
        ))
        .with_layout(&pipeline_layout)
        .with_subpass(subpass)
        .with_framebuffer_size(framebuffer_width, framebuffer_height)
        .with_face_culling(pso::Face::BACK)
        .with_depth_test(pso::DepthTest::On {
            fun: pso::Comparison::Less,
            write: !transparent,
        })
        .with_blend_targets(vec![pso::ColorBlendDesc(
            pso::ColorMask::ALL,
            if transparent {
                pso::BlendState::ALPHA
            } else {
                pso::BlendState::Off
            },
        )]);

    let pipelines = if skinning {
        let shader_vertex_skinned = unsafe { T::vertex_skinned_shader().module(factory).unwrap() };

        let vertex_desc = vertex_format_skinned
            .iter()
            .map(|f| (f.clone(), pso::VertexInputRate::Vertex))
            .chain(Some((
                SkinnedVertexArgs::vertex(),
                pso::VertexInputRate::Instance(1),
            )))
            .collect::<Vec<_>>();

        let pipe = PipelinesBuilder::new()
            .with_pipeline(pipe_desc.clone())
            .with_child_pipeline(
                0,
                pipe_desc
                    .with_vertex_desc(&vertex_desc)
                    .with_shaders(util::simple_shader_set(
                        &shader_vertex_skinned,
                        Some(&shader_fragment),
                    )),
            )
            .build(factory, None);

        unsafe {
            factory.destroy_shader_module(shader_vertex_skinned);
        }

        pipe
    } else {
        PipelinesBuilder::new()
            .with_pipeline(pipe_desc)
            .build(factory, None)
    };

    unsafe {
        factory.destroy_shader_module(shader_vertex_basic);
        factory.destroy_shader_module(shader_fragment);
    }

    match pipelines {
        Err(e) => {
            unsafe {
                factory.device().destroy_pipeline_layout(pipeline_layout);
            }
            Err(e)
        }
        Ok(pipelines) => Ok((pipelines, pipeline_layout)),
    }
}
