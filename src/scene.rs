use crate::{
    camera::CameraController,
    command::Command,
    interaction::navmesh::{
        data_model::{Navmesh, NavmeshEdge, NavmeshEntity, NavmeshTriangle, NavmeshVertex},
        selection::NavmeshSelection,
    },
    physics::{Collider, Joint, Physics, RigidBody},
    GameEngine, Message,
};
use rg3d::scene::base::{LevelOfDetail, LodGroup};
use rg3d::{
    animation::Animation,
    core::{
        algebra::{UnitQuaternion, Vector3},
        color::Color,
        math::Matrix4Ext,
        numeric_range::NumericRange,
        pool::{ErasedHandle, Handle, Pool, Ticket},
        visitor::{Visit, Visitor},
    },
    engine::resource_manager::ResourceManager,
    resource::texture::Texture,
    scene::{
        base::PhysicsBinding,
        graph::{Graph, SubGraph},
        mesh::{Mesh, RenderPath},
        node::Node,
        particle_system::{Emitter, ParticleLimit, ParticleSystem},
        physics::{ColliderShapeDesc, JointParamsDesc},
        Scene,
    },
    sound::math::TriangleDefinition,
};
use std::{collections::HashMap, fmt::Write, path::PathBuf, sync::mpsc::Sender};

pub struct Clipboard {
    graph: Graph,
    physics: Physics,
    empty: bool,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            graph: Graph::new(),
            physics: Default::default(),
            empty: true,
        }
    }
}

#[derive(Default, Debug)]
pub struct DeepCloneResult {
    root_nodes: Vec<Handle<Node>>,
    colliders: Vec<Handle<Collider>>,
    bodies: Vec<Handle<RigidBody>>,
    joints: Vec<Handle<Joint>>,
    binder: HashMap<Handle<Node>, Handle<RigidBody>>,
}

fn deep_clone_nodes(
    root_nodes: &[Handle<Node>],
    source_graph: &Graph,
    source_physics: &Physics,
    dest_graph: &mut Graph,
    dest_physics: &mut Physics,
) -> DeepCloneResult {
    let mut result = DeepCloneResult::default();

    let mut old_new_mapping = HashMap::new();

    for &root_node in root_nodes.iter() {
        let (_, old_to_new) = source_graph.copy_node(root_node, dest_graph, &mut |_, _| true);
        // Merge mappings.
        for (old, new) in old_to_new {
            old_new_mapping.insert(old, new);
        }
    }

    result.root_nodes = root_nodes
        .iter()
        .map(|n| *old_new_mapping.get(n).unwrap())
        .collect::<Vec<_>>();

    // Copy associated bodies, colliders, joints.
    for &root_node in root_nodes.iter() {
        for descendant in source_graph.traverse_handle_iter(root_node) {
            // Copy body too if we have any.
            if let Some(&body) = source_physics.binder.value_of(&descendant) {
                let body = &source_physics.bodies[body];
                let mut body_clone = body.clone();
                body_clone.colliders.clear();
                let body_clone_handle = dest_physics.bodies.spawn(body_clone);

                result.bodies.push(body_clone_handle);

                // Also copy colliders.
                for &collider in body.colliders.iter() {
                    let mut collider_clone = source_physics.colliders[collider.into()].clone();
                    collider_clone.parent = body_clone_handle.into();
                    let collider_clone_handle = dest_physics.colliders.spawn(collider_clone);
                    dest_physics.bodies[body_clone_handle]
                        .colliders
                        .push(collider_clone_handle.into());

                    result.colliders.push(collider_clone_handle);
                }

                let new_node = *old_new_mapping.get(&descendant).unwrap();
                result.binder.insert(new_node, body_clone_handle);
                dest_physics.binder.insert(new_node, body_clone_handle);
            }
        }
    }

    // TODO: Add joints.
    // Joint will be copied only if both of its associated bodies are copied too.

    result
}

impl Clipboard {
    pub fn fill_from_selection(
        &mut self,
        selection: &GraphSelection,
        scene_handle: Handle<Scene>,
        physics: &Physics,
        engine: &GameEngine,
    ) {
        self.clear();

        let scene = &engine.scenes[scene_handle];

        let root_nodes = selection.root_nodes(&scene.graph);

        deep_clone_nodes(
            &root_nodes,
            &scene.graph,
            physics,
            &mut self.graph,
            &mut self.physics,
        );

        self.empty = false;
    }

    pub fn paste(&mut self, dest_graph: &mut Graph, dest_physics: &mut Physics) -> DeepCloneResult {
        assert_ne!(self.empty, true);

        deep_clone_nodes(
            self.graph[self.graph.get_root()].children(),
            &self.graph,
            &self.physics,
            dest_graph,
            dest_physics,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }

    pub fn clear(&mut self) {
        self.empty = true;
        self.graph = Graph::new();
        self.physics = Default::default();
    }
}

pub struct EditorScene {
    pub path: Option<PathBuf>,
    pub scene: Handle<Scene>,
    // Handle to a root for all editor nodes.
    pub root: Handle<Node>,
    pub selection: Selection,
    pub clipboard: Clipboard,
    pub camera_controller: CameraController,
    // Editor uses split data model - some parts of scene are editable directly,
    // but some parts are not because of incompatible data model.
    pub physics: Physics,
    pub navmeshes: Pool<Navmesh>,
}

impl EditorScene {
    pub fn save(&mut self, path: PathBuf, engine: &mut GameEngine) -> Result<String, String> {
        let scene = &mut engine.scenes[self.scene];

        // Validate first.
        let mut valid = true;
        let mut reason = "Scene is not saved, because validation failed:\n".to_owned();

        for joint in self.physics.joints.iter() {
            if joint.body1.is_none() || joint.body2.is_none() {
                let mut associated_node = Handle::NONE;
                for (&node, &body) in self.physics.binder.forward_map().iter() {
                    if body == joint.body1.into() {
                        associated_node = node;
                        break;
                    }
                }

                writeln!(
                    &mut reason,
                    "Invalid joint on node {} ({}:{}). Associated body is missing!",
                    scene.graph[associated_node].name(),
                    associated_node.index(),
                    associated_node.generation()
                )
                .unwrap();
                valid = false;
            }
        }

        if valid {
            self.path = Some(path.clone());

            let editor_root = self.root;
            let (mut pure_scene, old_to_new) = scene.clone(&mut |node, _| node != editor_root);

            // Reset state of nodes. For some nodes (such as particles systems) we use scene as preview
            // so before saving scene, we have to reset state of such nodes.
            for node in pure_scene.graph.linear_iter_mut() {
                if let Node::ParticleSystem(particle_system) = node {
                    // Particle system must not save generated vertices.
                    particle_system.clear_particles();
                }
            }

            pure_scene.navmeshes.clear();

            for navmesh in self.navmeshes.iter() {
                // Sparse-to-dense mapping - handle to index.
                let mut vertex_map = HashMap::new();

                let vertices = navmesh
                    .vertices
                    .pair_iter()
                    .enumerate()
                    .map(|(i, (handle, vertex))| {
                        vertex_map.insert(handle, i);
                        vertex.position
                    })
                    .collect::<Vec<_>>();

                let triangles = navmesh
                    .triangles
                    .iter()
                    .map(|triangle| {
                        TriangleDefinition([
                            vertex_map[&triangle.a] as u32,
                            vertex_map[&triangle.b] as u32,
                            vertex_map[&triangle.c] as u32,
                        ])
                    })
                    .collect::<Vec<_>>();

                pure_scene
                    .navmeshes
                    .add(rg3d::utils::navmesh::Navmesh::new(&triangles, &vertices));
            }

            let (desc, binder) = self.physics.generate_engine_desc();
            pure_scene.physics.desc = Some(desc);
            pure_scene.physics_binder.enabled = true;
            pure_scene.physics_binder.clear();
            for (node, body) in binder {
                pure_scene
                    .physics_binder
                    .bind(*old_to_new.get(&node).unwrap(), body);
            }
            let mut visitor = Visitor::new();
            pure_scene.visit("Scene", &mut visitor).unwrap();
            if let Err(e) = visitor.save_binary(&path) {
                Err(format!("Failed to save scene! Reason: {}", e.to_string()))
            } else {
                Ok(format!("Scene {} was successfully saved!", path.display()))
            }
        } else {
            writeln!(&mut reason, "\nPlease fix errors and try again.").unwrap();

            Err(reason)
        }
    }
}

#[derive(Debug)]
pub enum SceneCommand {
    CommandGroup(CommandGroup),
    Paste(PasteCommand),
    AddNode(AddNodeCommand),
    DeleteNode(DeleteNodeCommand),
    DeleteSubGraph(DeleteSubGraphCommand),
    ChangeSelection(ChangeSelectionCommand),
    MoveNode(MoveNodeCommand),
    ScaleNode(ScaleNodeCommand),
    RotateNode(RotateNodeCommand),
    LinkNodes(LinkNodesCommand),
    SetVisible(SetVisibleCommand),
    SetName(SetNameCommand),
    SetLodGroup(SetLodGroupCommand),
    AddLodGroupLevel(AddLodGroupLevelCommand),
    RemoveLodGroupLevel(RemoveLodGroupLevelCommand),
    AddLodObject(AddLodObjectCommand),
    RemoveLodObject(RemoveLodObjectCommand),
    ChangeLodRangeEnd(ChangeLodRangeEndCommand),
    ChangeLodRangeBegin(ChangeLodRangeBeginCommand),
    SetTag(SetTagCommand),
    AddJoint(AddJointCommand),
    DeleteJoint(DeleteJointCommand),
    SetJointConnectedBody(SetJointConnectedBodyCommand),
    SetBody(SetBodyCommand),
    SetBodyMass(SetBodyMassCommand),
    SetCollider(SetColliderCommand),
    SetColliderFriction(SetColliderFrictionCommand),
    SetColliderRestitution(SetColliderRestitutionCommand),
    SetColliderPosition(SetColliderPositionCommand),
    SetColliderRotation(SetColliderRotationCommand),
    SetColliderIsSensor(SetColliderIsSensorCommand),
    SetColliderCollisionGroups(SetColliderCollisionGroupsCommand),
    SetCylinderHalfHeight(SetCylinderHalfHeightCommand),
    SetCylinderRadius(SetCylinderRadiusCommand),
    SetCapsuleRadius(SetCapsuleRadiusCommand),
    SetCapsuleBegin(SetCapsuleBeginCommand),
    SetCapsuleEnd(SetCapsuleEndCommand),
    SetConeHalfHeight(SetConeHalfHeightCommand),
    SetConeRadius(SetConeRadiusCommand),
    SetBallRadius(SetBallRadiusCommand),
    SetBallJointAnchor1(SetBallJointAnchor1Command),
    SetBallJointAnchor2(SetBallJointAnchor2Command),
    SetFixedJointAnchor1Translation(SetFixedJointAnchor1TranslationCommand),
    SetFixedJointAnchor2Translation(SetFixedJointAnchor2TranslationCommand),
    SetFixedJointAnchor1Rotation(SetFixedJointAnchor1RotationCommand),
    SetFixedJointAnchor2Rotation(SetFixedJointAnchor2RotationCommand),
    SetRevoluteJointAnchor1(SetRevoluteJointAnchor1Command),
    SetRevoluteJointAxis1(SetRevoluteJointAxis1Command),
    SetRevoluteJointAnchor2(SetRevoluteJointAnchor2Command),
    SetRevoluteJointAxis2(SetRevoluteJointAxis2Command),
    SetPrismaticJointAnchor1(SetPrismaticJointAnchor1Command),
    SetPrismaticJointAxis1(SetPrismaticJointAxis1Command),
    SetPrismaticJointAnchor2(SetPrismaticJointAnchor2Command),
    SetPrismaticJointAxis2(SetPrismaticJointAxis2Command),
    SetCuboidHalfExtents(SetCuboidHalfExtentsCommand),
    DeleteBody(DeleteBodyCommand),
    DeleteCollider(DeleteColliderCommand),
    LoadModel(LoadModelCommand),
    SetLightColor(SetLightColorCommand),
    SetLightScatter(SetLightScatterCommand),
    SetLightScatterEnabled(SetLightScatterEnabledCommand),
    SetLightCastShadows(SetLightCastShadowsCommand),
    SetPointLightRadius(SetPointLightRadiusCommand),
    SetSpotLightHotspot(SetSpotLightHotspotCommand),
    SetSpotLightFalloffAngleDelta(SetSpotLightFalloffAngleDeltaCommand),
    SetSpotLightDistance(SetSpotLightDistanceCommand),
    SetFov(SetFovCommand),
    SetZNear(SetZNearCommand),
    SetZFar(SetZFarCommand),
    SetParticleSystemAcceleration(SetParticleSystemAccelerationCommand),
    AddParticleSystemEmitter(AddParticleSystemEmitterCommand),
    SetEmitterNumericParameter(SetEmitterNumericParameterCommand),
    SetSphereEmitterRadius(SetSphereEmitterRadiusCommand),
    SetCylinderEmitterRadius(SetCylinderEmitterRadiusCommand),
    SetCylinderEmitterHeight(SetCylinderEmitterHeightCommand),
    SetBoxEmitterHalfWidth(SetBoxEmitterHalfWidthCommand),
    SetBoxEmitterHalfHeight(SetBoxEmitterHalfHeightCommand),
    SetBoxEmitterHalfDepth(SetBoxEmitterHalfDepthCommand),
    SetEmitterPosition(SetEmitterPositionCommand),
    SetParticleSystemTexture(SetParticleSystemTextureCommand),
    DeleteEmitter(DeleteEmitterCommand),
    SetSpriteSize(SetSpriteSizeCommand),
    SetSpriteRotation(SetSpriteRotationCommand),
    SetSpriteColor(SetSpriteColorCommand),
    SetSpriteTexture(SetSpriteTextureCommand),
    SetMeshTexture(SetMeshTextureCommand),
    SetMeshCastShadows(SetMeshCastShadowsCommand),
    SetMeshRenderPath(SetMeshRenderPathCommand),
    AddNavmesh(AddNavmeshCommand),
    DeleteNavmesh(DeleteNavmeshCommand),
    MoveNavmeshVertex(MoveNavmeshVertexCommand),
    AddNavmeshTriangle(AddNavmeshTriangleCommand),
    AddNavmeshVertex(AddNavmeshVertexCommand),
    AddNavmeshEdge(AddNavmeshEdgeCommand),
    DeleteNavmeshVertex(DeleteNavmeshVertexCommand),
    ConnectNavmeshEdges(ConnectNavmeshEdgesCommand),
    SetPhysicsBinding(SetPhysicsBindingCommand),
}

pub struct SceneContext<'a> {
    pub editor_scene: &'a mut EditorScene,
    pub scene: &'a mut Scene,
    pub message_sender: Sender<Message>,
    pub resource_manager: ResourceManager,
}

macro_rules! static_dispatch {
    ($self:ident, $func:ident, $($args:expr),*) => {
        match $self {
            SceneCommand::CommandGroup(v) => v.$func($($args),*),
            SceneCommand::Paste(v) => v.$func($($args),*),
            SceneCommand::AddNode(v) => v.$func($($args),*),
            SceneCommand::DeleteNode(v) => v.$func($($args),*),
            SceneCommand::ChangeSelection(v) => v.$func($($args),*),
            SceneCommand::MoveNode(v) => v.$func($($args),*),
            SceneCommand::ScaleNode(v) => v.$func($($args),*),
            SceneCommand::RotateNode(v) => v.$func($($args),*),
            SceneCommand::LinkNodes(v) => v.$func($($args),*),
            SceneCommand::SetVisible(v) => v.$func($($args),*),
            SceneCommand::SetName(v) => v.$func($($args),*),
            SceneCommand::SetLodGroup(v) => v.$func($($args),*),
            SceneCommand::AddLodGroupLevel(v) => v.$func($($args),*),
            SceneCommand::RemoveLodGroupLevel(v) => v.$func($($args),*),
            SceneCommand::AddLodObject(v) => v.$func($($args),*),
            SceneCommand::RemoveLodObject(v) => v.$func($($args),*),
            SceneCommand::ChangeLodRangeEnd(v) => v.$func($($args),*),
            SceneCommand::ChangeLodRangeBegin(v) => v.$func($($args),*),
            SceneCommand::SetTag(v) => v.$func($($args),*),
            SceneCommand::SetBody(v) => v.$func($($args),*),
            SceneCommand::AddJoint(v) => v.$func($($args),*),
            SceneCommand::SetJointConnectedBody(v) => v.$func($($args),*),
            SceneCommand::DeleteJoint(v) => v.$func($($args),*),
            SceneCommand::DeleteSubGraph(v) => v.$func($($args),*),
            SceneCommand::SetBodyMass(v) => v.$func($($args),*),
            SceneCommand::SetCollider(v) => v.$func($($args),*),
            SceneCommand::SetColliderFriction(v) => v.$func($($args),*),
            SceneCommand::SetColliderRestitution(v) => v.$func($($args),*),
            SceneCommand::SetColliderPosition(v) => v.$func($($args),*),
            SceneCommand::SetColliderRotation(v) => v.$func($($args),*),
            SceneCommand::SetColliderIsSensor(v) => v.$func($($args),*),
            SceneCommand::SetColliderCollisionGroups(v) => v.$func($($args),*),
            SceneCommand::SetCylinderHalfHeight(v) => v.$func($($args),*),
            SceneCommand::SetCylinderRadius(v) => v.$func($($args),*),
            SceneCommand::SetCapsuleRadius(v) => v.$func($($args),*),
            SceneCommand::SetCapsuleBegin(v) => v.$func($($args),*),
            SceneCommand::SetCapsuleEnd(v) => v.$func($($args),*),
            SceneCommand::SetConeHalfHeight(v) => v.$func($($args),*),
            SceneCommand::SetConeRadius(v) => v.$func($($args),*),
            SceneCommand::SetBallRadius(v) => v.$func($($args),*),
            SceneCommand::SetBallJointAnchor1(v) => v.$func($($args),*),
            SceneCommand::SetBallJointAnchor2(v) => v.$func($($args),*),
            SceneCommand::SetFixedJointAnchor1Translation(v) => v.$func($($args),*),
            SceneCommand::SetFixedJointAnchor2Translation(v) => v.$func($($args),*),
            SceneCommand::SetFixedJointAnchor1Rotation(v) => v.$func($($args),*),
            SceneCommand::SetFixedJointAnchor2Rotation(v) => v.$func($($args),*),
            SceneCommand::SetRevoluteJointAnchor1(v) => v.$func($($args),*),
            SceneCommand::SetRevoluteJointAxis1(v) => v.$func($($args),*),
            SceneCommand::SetRevoluteJointAnchor2(v) => v.$func($($args),*),
            SceneCommand::SetRevoluteJointAxis2(v) => v.$func($($args),*),
            SceneCommand::SetPrismaticJointAnchor1(v) => v.$func($($args),*),
            SceneCommand::SetPrismaticJointAxis1(v) => v.$func($($args),*),
            SceneCommand::SetPrismaticJointAnchor2(v) => v.$func($($args),*),
            SceneCommand::SetPrismaticJointAxis2(v) => v.$func($($args),*),
            SceneCommand::SetCuboidHalfExtents(v) => v.$func($($args),*),
            SceneCommand::DeleteBody(v) => v.$func($($args),*),
            SceneCommand::DeleteCollider(v) => v.$func($($args),*),
            SceneCommand::LoadModel(v) => v.$func($($args),*),
            SceneCommand::SetLightColor(v) => v.$func($($args),*),
            SceneCommand::SetLightScatter(v) => v.$func($($args),*),
            SceneCommand::SetLightScatterEnabled(v) => v.$func($($args),*),
            SceneCommand::SetLightCastShadows(v) => v.$func($($args),*),
            SceneCommand::SetPointLightRadius(v) => v.$func($($args),*),
            SceneCommand::SetSpotLightHotspot(v) => v.$func($($args),*),
            SceneCommand::SetSpotLightFalloffAngleDelta(v) => v.$func($($args),*),
            SceneCommand::SetSpotLightDistance(v) => v.$func($($args),*),
            SceneCommand::SetFov(v) => v.$func($($args),*),
            SceneCommand::SetZNear(v) => v.$func($($args),*),
            SceneCommand::SetZFar(v) => v.$func($($args),*),
            SceneCommand::SetParticleSystemAcceleration(v) => v.$func($($args),*),
            SceneCommand::AddParticleSystemEmitter(v) => v.$func($($args),*),
            SceneCommand::SetEmitterNumericParameter(v) => v.$func($($args),*),
            SceneCommand::SetSphereEmitterRadius(v) => v.$func($($args),*),
            SceneCommand::SetEmitterPosition(v) => v.$func($($args),*),
            SceneCommand::SetParticleSystemTexture(v) => v.$func($($args),*),
            SceneCommand::SetCylinderEmitterRadius(v) => v.$func($($args),*),
            SceneCommand::SetCylinderEmitterHeight(v) => v.$func($($args),*),
            SceneCommand::SetBoxEmitterHalfWidth(v) => v.$func($($args),*),
            SceneCommand::SetBoxEmitterHalfHeight(v) => v.$func($($args),*),
            SceneCommand::SetBoxEmitterHalfDepth(v) => v.$func($($args),*),
            SceneCommand::DeleteEmitter(v) => v.$func($($args),*),
            SceneCommand::SetSpriteSize(v) => v.$func($($args),*),
            SceneCommand::SetSpriteRotation(v) => v.$func($($args),*),
            SceneCommand::SetSpriteColor(v) => v.$func($($args),*),
            SceneCommand::SetSpriteTexture(v) => v.$func($($args),*),
            SceneCommand::SetMeshTexture(v) => v.$func($($args),*),
            SceneCommand::SetMeshCastShadows(v) => v.$func($($args),*),
            SceneCommand::SetMeshRenderPath(v) => v.$func($($args),*),
            SceneCommand::AddNavmesh(v) => v.$func($($args),*),
            SceneCommand::DeleteNavmesh(v) => v.$func($($args),*),
            SceneCommand::MoveNavmeshVertex(v) => v.$func($($args),*),
            SceneCommand::AddNavmeshVertex(v) => v.$func($($args),*),
            SceneCommand::AddNavmeshTriangle(v) => v.$func($($args),*),
            SceneCommand::AddNavmeshEdge(v) => v.$func($($args),*),
            SceneCommand::DeleteNavmeshVertex(v) => v.$func($($args),*),
            SceneCommand::ConnectNavmeshEdges(v) => v.$func($($args),*),
            SceneCommand::SetPhysicsBinding(v) => v.$func($($args),*),
        }
    };
}

#[derive(Debug)]
pub struct CommandGroup {
    commands: Vec<SceneCommand>,
}

impl From<Vec<SceneCommand>> for CommandGroup {
    fn from(commands: Vec<SceneCommand>) -> Self {
        Self { commands }
    }
}

impl CommandGroup {
    pub fn push(&mut self, command: SceneCommand) {
        self.commands.push(command)
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl<'a> Command<'a> for CommandGroup {
    type Context = SceneContext<'a>;

    fn name(&mut self, context: &Self::Context) -> String {
        let mut name = String::from("Command group: ");
        for cmd in self.commands.iter_mut() {
            name.push_str(&cmd.name(context));
            name.push_str(", ");
        }
        name
    }

    fn execute(&mut self, context: &mut Self::Context) {
        for cmd in self.commands.iter_mut() {
            cmd.execute(context);
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        // revert must be done in reverse order.
        for cmd in self.commands.iter_mut().rev() {
            cmd.revert(context);
        }
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        for mut cmd in self.commands.drain(..) {
            cmd.finalize(context);
        }
    }
}

impl<'a> Command<'a> for SceneCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, context: &Self::Context) -> String {
        static_dispatch!(self, name, context)
    }

    fn execute(&mut self, context: &mut Self::Context) {
        static_dispatch!(self, execute, context);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        static_dispatch!(self, revert, context);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        static_dispatch!(self, finalize, context);
    }
}

#[derive(Debug)]
pub struct AddNodeCommand {
    ticket: Option<Ticket<Node>>,
    handle: Handle<Node>,
    node: Option<Node>,
    cached_name: String,
}

impl AddNodeCommand {
    pub fn new(node: Node) -> Self {
        Self {
            ticket: None,
            handle: Default::default(),
            cached_name: format!("Add Node {}", node.name()),
            node: Some(node),
        }
    }
}

impl<'a> Command<'a> for AddNodeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        self.cached_name.clone()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match self.ticket.take() {
            None => {
                self.handle = context.scene.graph.add_node(self.node.take().unwrap());
            }
            Some(ticket) => {
                let handle = context
                    .scene
                    .graph
                    .put_back(ticket, self.node.take().unwrap());
                assert_eq!(handle, self.handle);
            }
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context.scene.graph.take_reserve(self.handle);
        self.ticket = Some(ticket);
        self.node = Some(node);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.scene.graph.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct AddParticleSystemEmitterCommand {
    particle_system: Handle<Node>,
    emitter: Option<Emitter>,
}

impl AddParticleSystemEmitterCommand {
    pub fn new(particle_system: Handle<Node>, emitter: Emitter) -> Self {
        Self {
            particle_system,
            emitter: Some(emitter),
        }
    }
}

impl<'a> Command<'a> for AddParticleSystemEmitterCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Particle System Emitter".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        context.scene.graph[self.particle_system]
            .as_particle_system_mut()
            .emitters
            .push(self.emitter.take().unwrap());
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.emitter = Some(
            context.scene.graph[self.particle_system]
                .as_particle_system_mut()
                .emitters
                .pop()
                .unwrap(),
        );
    }
}

#[derive(Debug)]
pub struct AddNavmeshEdgeCommand {
    navmesh: Handle<Navmesh>,
    opposite_edge: NavmeshEdge,
    state: AddNavmeshEdgeCommandState,
    select: bool,
    new_selection: Selection,
}

#[derive(Debug)]
enum AddNavmeshEdgeCommandState {
    Undefined,
    NonExecuted {
        edge: (NavmeshVertex, NavmeshVertex),
    },
    Executed {
        triangles: [Handle<NavmeshTriangle>; 2],
        vertices: [Handle<NavmeshVertex>; 2],
    },
    Reverted {
        triangles: [(Ticket<NavmeshTriangle>, NavmeshTriangle); 2],
        vertices: [(Ticket<NavmeshVertex>, NavmeshVertex); 2],
    },
}

impl AddNavmeshEdgeCommand {
    pub fn new(
        navmesh: Handle<Navmesh>,
        edge: (NavmeshVertex, NavmeshVertex),
        opposite_edge: NavmeshEdge,
        select: bool,
    ) -> Self {
        Self {
            navmesh,
            opposite_edge,
            state: AddNavmeshEdgeCommandState::NonExecuted { edge },
            select,
            new_selection: Default::default(),
        }
    }
}

impl<'a> Command<'a> for AddNavmeshEdgeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Navmesh Edge".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];
        match std::mem::replace(&mut self.state, AddNavmeshEdgeCommandState::Undefined) {
            AddNavmeshEdgeCommandState::NonExecuted { edge } => {
                let begin_handle = navmesh.vertices.spawn(edge.0);
                let end_handle = navmesh.vertices.spawn(edge.1);
                let triangle_a = navmesh.triangles.spawn(NavmeshTriangle {
                    a: self.opposite_edge.begin,
                    b: begin_handle,
                    c: self.opposite_edge.end,
                });
                let triangle_b = navmesh.triangles.spawn(NavmeshTriangle {
                    a: begin_handle,
                    b: end_handle,
                    c: self.opposite_edge.end,
                });
                self.state = AddNavmeshEdgeCommandState::Executed {
                    triangles: [triangle_a, triangle_b],
                    vertices: [begin_handle, end_handle],
                };

                let navmesh_selection = NavmeshSelection::new(
                    self.navmesh,
                    vec![NavmeshEntity::Edge(NavmeshEdge {
                        begin: begin_handle,
                        end: end_handle,
                    })],
                );

                self.new_selection = Selection::Navmesh(navmesh_selection);
            }
            AddNavmeshEdgeCommandState::Reverted {
                triangles,
                vertices,
            } => {
                let [va, vb] = vertices;
                let begin_handle = navmesh.vertices.put_back(va.0, va.1);
                let end_handle = navmesh.vertices.put_back(vb.0, vb.1);

                let [ta, tb] = triangles;
                let triangle_a = navmesh.triangles.put_back(ta.0, ta.1);
                let triangle_b = navmesh.triangles.put_back(tb.0, tb.1);

                self.state = AddNavmeshEdgeCommandState::Executed {
                    triangles: [triangle_a, triangle_b],
                    vertices: [begin_handle, end_handle],
                };
            }
            _ => unreachable!(),
        }

        if self.select {
            std::mem::swap(&mut context.editor_scene.selection, &mut self.new_selection);
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        if self.select {
            std::mem::swap(&mut context.editor_scene.selection, &mut self.new_selection);
        }

        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];
        match std::mem::replace(&mut self.state, AddNavmeshEdgeCommandState::Undefined) {
            AddNavmeshEdgeCommandState::Executed {
                triangles,
                vertices,
            } => {
                self.state = AddNavmeshEdgeCommandState::Reverted {
                    triangles: [
                        navmesh.triangles.take_reserve(triangles[0]),
                        navmesh.triangles.take_reserve(triangles[1]),
                    ],
                    vertices: [
                        navmesh.vertices.take_reserve(vertices[0]),
                        navmesh.vertices.take_reserve(vertices[1]),
                    ],
                };
            }
            _ => unreachable!(),
        }
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let AddNavmeshEdgeCommandState::Reverted {
            triangles,
            vertices,
        } = std::mem::replace(&mut self.state, AddNavmeshEdgeCommandState::Undefined)
        {
            if let Some(navmesh) = context.editor_scene.navmeshes.try_borrow_mut(self.navmesh) {
                // Forget tickets.
                let [va, vb] = vertices;
                navmesh.vertices.forget_ticket(va.0);
                navmesh.vertices.forget_ticket(vb.0);

                let [ta, tb] = triangles;
                navmesh.triangles.forget_ticket(ta.0);
                navmesh.triangles.forget_ticket(tb.0);
            }
        }
    }
}

#[derive(Debug)]
pub enum ConnectNavmeshEdgesCommandState {
    Undefined,
    NonExecuted {
        edges: [NavmeshEdge; 2],
    },
    Executed {
        triangles: [Handle<NavmeshTriangle>; 2],
    },
    Reverted {
        triangles: [(Ticket<NavmeshTriangle>, NavmeshTriangle); 2],
    },
}

#[derive(Debug)]
pub struct ConnectNavmeshEdgesCommand {
    navmesh: Handle<Navmesh>,
    state: ConnectNavmeshEdgesCommandState,
}

impl ConnectNavmeshEdgesCommand {
    pub fn new(navmesh: Handle<Navmesh>, edges: [NavmeshEdge; 2]) -> Self {
        Self {
            navmesh,
            state: ConnectNavmeshEdgesCommandState::NonExecuted { edges },
        }
    }
}

impl<'a> Command<'a> for ConnectNavmeshEdgesCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Connect Navmesh Edges".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];

        match std::mem::replace(&mut self.state, ConnectNavmeshEdgesCommandState::Undefined) {
            ConnectNavmeshEdgesCommandState::NonExecuted { edges } => {
                let ta = navmesh.triangles.spawn(NavmeshTriangle {
                    a: edges[0].begin,
                    b: edges[0].end,
                    c: edges[1].begin,
                });
                let tb = navmesh.triangles.spawn(NavmeshTriangle {
                    a: edges[1].begin,
                    b: edges[1].end,
                    c: edges[0].begin,
                });

                self.state = ConnectNavmeshEdgesCommandState::Executed {
                    triangles: [ta, tb],
                };
            }
            ConnectNavmeshEdgesCommandState::Reverted { triangles } => {
                let [a, b] = triangles;
                let ta = navmesh.triangles.put_back(a.0, a.1);
                let tb = navmesh.triangles.put_back(b.0, b.1);

                self.state = ConnectNavmeshEdgesCommandState::Executed {
                    triangles: [ta, tb],
                }
            }
            _ => unreachable!(),
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];

        match std::mem::replace(&mut self.state, ConnectNavmeshEdgesCommandState::Undefined) {
            ConnectNavmeshEdgesCommandState::Executed { triangles } => {
                self.state = ConnectNavmeshEdgesCommandState::Reverted {
                    triangles: [
                        navmesh.triangles.take_reserve(triangles[0]),
                        navmesh.triangles.take_reserve(triangles[1]),
                    ],
                }
            }
            _ => unreachable!(),
        }
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];

        if let ConnectNavmeshEdgesCommandState::Reverted { triangles } =
            std::mem::replace(&mut self.state, ConnectNavmeshEdgesCommandState::Undefined)
        {
            let [a, b] = triangles;
            navmesh.triangles.forget_ticket(a.0);
            navmesh.triangles.forget_ticket(b.0);
        }
    }
}

#[derive(Debug)]
pub struct DeleteEmitterCommand {
    particle_system: Handle<Node>,
    emitter: Option<Emitter>,
    emitter_index: usize,
}

impl DeleteEmitterCommand {
    pub fn new(particle_system: Handle<Node>, emitter_index: usize) -> Self {
        Self {
            particle_system,
            emitter: None,
            emitter_index,
        }
    }
}

impl<'a> Command<'a> for DeleteEmitterCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Particle System Emitter".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.emitter = Some(
            context.scene.graph[self.particle_system]
                .as_particle_system_mut()
                .emitters
                .remove(self.emitter_index),
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let particle_system: &mut ParticleSystem =
            context.scene.graph[self.particle_system].as_particle_system_mut();
        if self.emitter_index == 0 {
            particle_system.emitters.push(self.emitter.take().unwrap());
        } else {
            particle_system
                .emitters
                .insert(self.emitter_index, self.emitter.take().unwrap());
        }
    }
}

#[derive(Debug)]
pub struct AddNavmeshCommand {
    ticket: Option<Ticket<Navmesh>>,
    handle: Handle<Navmesh>,
    navmesh: Option<Navmesh>,
}

impl AddNavmeshCommand {
    pub fn new(navmesh: Navmesh) -> Self {
        Self {
            ticket: None,
            handle: Default::default(),
            navmesh: Some(navmesh),
        }
    }
}

impl<'a> Command<'a> for AddNavmeshCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Navmesh".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match self.ticket.take() {
            None => {
                self.handle = context
                    .editor_scene
                    .navmeshes
                    .spawn(self.navmesh.take().unwrap());
            }
            Some(ticket) => {
                let handle = context
                    .editor_scene
                    .navmeshes
                    .put_back(ticket, self.navmesh.take().unwrap());
                assert_eq!(handle, self.handle);
            }
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context.editor_scene.navmeshes.take_reserve(self.handle);
        self.ticket = Some(ticket);
        self.navmesh = Some(node);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.navmeshes.forget_ticket(ticket)
        }
    }
}

macro_rules! define_pool_command {
    ($name:ident, $inner_ty:ty, $human_readable_name:expr, $ctx:ident, $self:ident, $get_pool:block, $($field:ident:$type:ty),*) => {
        #[derive(Debug)]
        pub struct $name {
            pub ticket: Option<Ticket<$inner_ty>>,
            pub handle: Handle<$inner_ty>,
            pub value: Option<$inner_ty>,
            $(pub $field: $type,)*
        }

        impl<'a> Command<'a> for $name {
            type Context = SceneContext<'a>;

            fn name(&mut self, _context: &Self::Context) -> String {
                $human_readable_name.to_owned()
            }

            fn execute(&mut $self, $ctx: &mut Self::Context) {
               let pool = $get_pool;
               match $self.ticket.take() {
                    None => {
                        $self.handle = pool.spawn($self.value.take().unwrap());
                    }
                    Some(ticket) => {
                        let handle = pool.put_back(ticket, $self.value.take().unwrap());
                        assert_eq!(handle, $self.handle);
                    }
                }
            }

            fn revert(&mut $self, $ctx: &mut Self::Context) {
                let pool = $get_pool;

                let (ticket, node) = pool.take_reserve($self.handle);
                $self.ticket = Some(ticket);
                $self.value = Some(node);
            }

            fn finalize(&mut $self, $ctx: &mut Self::Context) {
                let pool = $get_pool;

                if let Some(ticket) = $self.ticket.take() {
                    pool.forget_ticket(ticket)
                }
            }
        }
    };
}

define_pool_command!(
    AddNavmeshVertexCommand,
    NavmeshVertex,
    "Add Navmesh Vertex",
    ctx,
    self,
    { &mut ctx.editor_scene.navmeshes[self.navmesh].vertices },
    navmesh: Handle<Navmesh>
);

define_pool_command!(
    AddNavmeshTriangleCommand,
    NavmeshTriangle,
    "Add Navmesh Triangle",
    ctx,
    self,
    { &mut ctx.editor_scene.navmeshes[self.navmesh].triangles },
    navmesh: Handle<Navmesh>
);

#[derive(Debug)]
pub struct DeleteNavmeshCommand {
    handle: Handle<Navmesh>,
    ticket: Option<Ticket<Navmesh>>,
    node: Option<Navmesh>,
}

impl DeleteNavmeshCommand {
    pub fn new(handle: Handle<Navmesh>) -> Self {
        Self {
            handle,
            ticket: None,
            node: None,
        }
    }
}

impl<'a> Command<'a> for DeleteNavmeshCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Navmesh".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context.editor_scene.navmeshes.take_reserve(self.handle);
        self.node = Some(node);
        self.ticket = Some(ticket);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.handle = context
            .editor_scene
            .navmeshes
            .put_back(self.ticket.take().unwrap(), self.node.take().unwrap());
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.navmeshes.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct DeleteNavmeshVertexCommand {
    navmesh: Handle<Navmesh>,
    state: DeleteNavmeshVertexCommandState,
}

#[derive(Debug)]
pub enum DeleteNavmeshVertexCommandState {
    Undefined,
    NonExecuted {
        vertex: Handle<NavmeshVertex>,
    },
    Executed {
        vertex: (Ticket<NavmeshVertex>, NavmeshVertex),
        triangles: Vec<(Ticket<NavmeshTriangle>, NavmeshTriangle)>,
    },
    Reverted {
        vertex: Handle<NavmeshVertex>,
    },
}

impl DeleteNavmeshVertexCommand {
    pub fn new(navmesh: Handle<Navmesh>, vertex: Handle<NavmeshVertex>) -> Self {
        Self {
            navmesh,
            state: DeleteNavmeshVertexCommandState::NonExecuted { vertex },
        }
    }
}

impl<'a> Command<'a> for DeleteNavmeshVertexCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Navmesh Vertex".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];

        match std::mem::replace(&mut self.state, DeleteNavmeshVertexCommandState::Undefined) {
            DeleteNavmeshVertexCommandState::NonExecuted { vertex }
            | DeleteNavmeshVertexCommandState::Reverted { vertex } => {
                // Find each triangle that shares the same vertex and move them out of pool.
                let mut triangles = Vec::new();
                for (handle, triangle) in navmesh.triangles.pair_iter() {
                    if triangle.vertices().contains(&vertex) {
                        triangles.push(handle);
                    }
                }

                self.state = DeleteNavmeshVertexCommandState::Executed {
                    vertex: navmesh.vertices.take_reserve(vertex),
                    triangles: triangles
                        .iter()
                        .map(|&t| navmesh.triangles.take_reserve(t))
                        .collect(),
                };
            }
            _ => unreachable!(),
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let navmesh = &mut context.editor_scene.navmeshes[self.navmesh];

        match std::mem::replace(&mut self.state, DeleteNavmeshVertexCommandState::Undefined) {
            DeleteNavmeshVertexCommandState::Executed { vertex, triangles } => {
                let vertex = navmesh.vertices.put_back(vertex.0, vertex.1);
                for (ticket, triangle) in triangles {
                    navmesh.triangles.put_back(ticket, triangle);
                }

                self.state = DeleteNavmeshVertexCommandState::Reverted { vertex };
            }
            _ => unreachable!(),
        }
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let DeleteNavmeshVertexCommandState::Executed { vertex, triangles } =
            std::mem::replace(&mut self.state, DeleteNavmeshVertexCommandState::Undefined)
        {
            if let Some(navmesh) = context.editor_scene.navmeshes.try_borrow_mut(self.navmesh) {
                navmesh.vertices.forget_ticket(vertex.0);
                for (ticket, _) in triangles {
                    navmesh.triangles.forget_ticket(ticket);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct AddJointCommand {
    ticket: Option<Ticket<Joint>>,
    handle: Handle<Joint>,
    joint: Option<Joint>,
}

impl AddJointCommand {
    pub fn new(node: Joint) -> Self {
        Self {
            ticket: None,
            handle: Default::default(),
            joint: Some(node),
        }
    }
}

impl<'a> Command<'a> for AddJointCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Joint".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match self.ticket.take() {
            None => {
                self.handle = context
                    .editor_scene
                    .physics
                    .joints
                    .spawn(self.joint.take().unwrap());
            }
            Some(ticket) => {
                let handle = context
                    .editor_scene
                    .physics
                    .joints
                    .put_back(ticket, self.joint.take().unwrap());
                assert_eq!(handle, self.handle);
            }
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context
            .editor_scene
            .physics
            .joints
            .take_reserve(self.handle);
        self.ticket = Some(ticket);
        self.joint = Some(node);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.joints.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct DeleteJointCommand {
    handle: Handle<Joint>,
    ticket: Option<Ticket<Joint>>,
    node: Option<Joint>,
}

impl DeleteJointCommand {
    pub fn new(handle: Handle<Joint>) -> Self {
        Self {
            handle,
            ticket: None,
            node: None,
        }
    }
}

impl<'a> Command<'a> for DeleteJointCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Joint".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context
            .editor_scene
            .physics
            .joints
            .take_reserve(self.handle);
        self.node = Some(node);
        self.ticket = Some(ticket);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.handle = context
            .editor_scene
            .physics
            .joints
            .put_back(self.ticket.take().unwrap(), self.node.take().unwrap());
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.joints.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct ChangeSelectionCommand {
    new_selection: Selection,
    old_selection: Selection,
    cached_name: String,
}

impl ChangeSelectionCommand {
    pub fn new(new_selection: Selection, old_selection: Selection) -> Self {
        Self {
            cached_name: match new_selection {
                Selection::None => "Change Selection: None",
                Selection::Graph(_) => "Change Selection: Graph",
                Selection::Navmesh(_) => "Change Selection: Navmesh",
            }
            .to_owned(),
            new_selection,
            old_selection,
        }
    }

    fn swap(&mut self) -> Selection {
        let selection = self.new_selection.clone();
        std::mem::swap(&mut self.new_selection, &mut self.old_selection);
        selection
    }
}

impl<'a> Command<'a> for ChangeSelectionCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        self.cached_name.clone()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let new_selection = self.swap();
        if new_selection != context.editor_scene.selection {
            context.editor_scene.selection = new_selection;
            context
                .message_sender
                .send(Message::SelectionChanged)
                .unwrap();
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let new_selection = self.swap();
        if new_selection != context.editor_scene.selection {
            context.editor_scene.selection = new_selection;
            context
                .message_sender
                .send(Message::SelectionChanged)
                .unwrap();
        }
    }
}

#[derive(Debug)]
enum PasteCommandState {
    Undefined,
    NonExecuted,
    Reverted {
        subgraphs: Vec<SubGraph>,
        bodies: Vec<(Ticket<RigidBody>, RigidBody)>,
        colliders: Vec<(Ticket<Collider>, Collider)>,
        joints: Vec<(Ticket<Joint>, Joint)>,
        binder: HashMap<Handle<Node>, Handle<RigidBody>>,
        selection: Selection,
    },
    Executed {
        paste_result: DeepCloneResult,
        last_selection: Selection,
    },
}

#[derive(Debug)]
pub struct PasteCommand {
    state: PasteCommandState,
}

impl Default for PasteCommand {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteCommand {
    pub fn new() -> Self {
        Self {
            state: PasteCommandState::NonExecuted,
        }
    }
}

impl<'a> Command<'a> for PasteCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Paste".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match std::mem::replace(&mut self.state, PasteCommandState::Undefined) {
            PasteCommandState::NonExecuted => {
                let paste_result = context
                    .editor_scene
                    .clipboard
                    .paste(&mut context.scene.graph, &mut context.editor_scene.physics);

                let mut selection =
                    Selection::Graph(GraphSelection::from_list(paste_result.root_nodes.clone()));
                std::mem::swap(&mut context.editor_scene.selection, &mut selection);

                self.state = PasteCommandState::Executed {
                    paste_result,
                    last_selection: selection,
                };
            }
            PasteCommandState::Reverted {
                subgraphs,
                bodies,
                colliders,
                joints,
                binder,
                mut selection,
            } => {
                let mut paste_result = DeepCloneResult {
                    binder,
                    ..Default::default()
                };

                for subgraph in subgraphs {
                    paste_result
                        .root_nodes
                        .push(context.scene.graph.put_sub_graph_back(subgraph));
                }

                for (ticket, body) in bodies {
                    paste_result
                        .bodies
                        .push(context.editor_scene.physics.bodies.put_back(ticket, body));
                }

                for (ticket, collider) in colliders {
                    paste_result.colliders.push(
                        context
                            .editor_scene
                            .physics
                            .colliders
                            .put_back(ticket, collider),
                    );
                }

                for (ticket, joint) in joints {
                    paste_result
                        .joints
                        .push(context.editor_scene.physics.joints.put_back(ticket, joint));
                }

                for (&node, &body) in paste_result.binder.iter() {
                    context.editor_scene.physics.binder.insert(node, body);
                }

                std::mem::swap(&mut context.editor_scene.selection, &mut selection);
                self.state = PasteCommandState::Executed {
                    paste_result,
                    last_selection: selection,
                };
            }
            _ => unreachable!(),
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        if let PasteCommandState::Executed {
            paste_result,
            mut last_selection,
        } = std::mem::replace(&mut self.state, PasteCommandState::Undefined)
        {
            let mut subgraphs = Vec::new();
            for root_node in paste_result.root_nodes {
                subgraphs.push(context.scene.graph.take_reserve_sub_graph(root_node));
            }

            let mut bodies = Vec::new();
            for body in paste_result.bodies {
                bodies.push(context.editor_scene.physics.bodies.take_reserve(body));
            }

            let mut colliders = Vec::new();
            for collider in paste_result.colliders {
                colliders.push(
                    context
                        .editor_scene
                        .physics
                        .colliders
                        .take_reserve(collider),
                );
            }

            let mut joints = Vec::new();
            for joint in paste_result.joints {
                joints.push(context.editor_scene.physics.joints.take_reserve(joint));
            }

            for (node, _) in paste_result.binder.iter() {
                context.editor_scene.physics.binder.remove_by_key(node);
            }

            std::mem::swap(&mut context.editor_scene.selection, &mut last_selection);

            self.state = PasteCommandState::Reverted {
                subgraphs,
                bodies,
                colliders,
                joints,
                binder: paste_result.binder,
                selection: last_selection,
            };
        }
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let PasteCommandState::Reverted {
            subgraphs,
            bodies,
            colliders,
            joints,
            ..
        } = std::mem::replace(&mut self.state, PasteCommandState::Undefined)
        {
            for subgraph in subgraphs {
                context.scene.graph.forget_sub_graph(subgraph);
            }

            for (ticket, _) in bodies {
                context.editor_scene.physics.bodies.forget_ticket(ticket);
            }

            for (ticket, _) in colliders {
                context.editor_scene.physics.colliders.forget_ticket(ticket)
            }

            for (ticket, _) in joints {
                context.editor_scene.physics.joints.forget_ticket(ticket);
            }
        }
    }
}

#[derive(Debug)]
pub struct MoveNavmeshVertexCommand {
    navmesh: Handle<Navmesh>,
    vertex: Handle<NavmeshVertex>,
    old_position: Vector3<f32>,
    new_position: Vector3<f32>,
}

impl MoveNavmeshVertexCommand {
    pub fn new(
        navmesh: Handle<Navmesh>,
        vertex: Handle<NavmeshVertex>,
        old_position: Vector3<f32>,
        new_position: Vector3<f32>,
    ) -> Self {
        Self {
            navmesh,
            vertex,
            old_position,
            new_position,
        }
    }

    fn swap(&mut self) -> Vector3<f32> {
        let position = self.new_position;
        std::mem::swap(&mut self.new_position, &mut self.old_position);
        position
    }

    fn set_position(&self, navmesh: &mut Navmesh, position: Vector3<f32>) {
        navmesh.vertices[self.vertex].position = position;
    }
}

impl<'a> Command<'a> for MoveNavmeshVertexCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Move Navmesh Vertex".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let position = self.swap();
        self.set_position(&mut context.editor_scene.navmeshes[self.navmesh], position);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let position = self.swap();
        self.set_position(&mut context.editor_scene.navmeshes[self.navmesh], position);
    }
}

#[derive(Debug)]
pub struct MoveNodeCommand {
    node: Handle<Node>,
    old_position: Vector3<f32>,
    new_position: Vector3<f32>,
}

impl MoveNodeCommand {
    pub fn new(node: Handle<Node>, old_position: Vector3<f32>, new_position: Vector3<f32>) -> Self {
        Self {
            node,
            old_position,
            new_position,
        }
    }

    fn swap(&mut self) -> Vector3<f32> {
        let position = self.new_position;
        std::mem::swap(&mut self.new_position, &mut self.old_position);
        position
    }

    fn set_position(&self, graph: &mut Graph, physics: &mut Physics, position: Vector3<f32>) {
        graph[self.node]
            .local_transform_mut()
            .set_position(position);
        if let Some(&body) = physics.binder.value_of(&self.node) {
            physics.bodies[body].position = position;
        }
    }
}

impl<'a> Command<'a> for MoveNodeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Move Node".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let position = self.swap();
        self.set_position(
            &mut context.scene.graph,
            &mut context.editor_scene.physics,
            position,
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let position = self.swap();
        self.set_position(
            &mut context.scene.graph,
            &mut context.editor_scene.physics,
            position,
        );
    }
}

#[derive(Debug)]
pub struct ScaleNodeCommand {
    node: Handle<Node>,
    old_scale: Vector3<f32>,
    new_scale: Vector3<f32>,
}

impl ScaleNodeCommand {
    pub fn new(node: Handle<Node>, old_scale: Vector3<f32>, new_scale: Vector3<f32>) -> Self {
        Self {
            node,
            old_scale,
            new_scale,
        }
    }

    fn swap(&mut self) -> Vector3<f32> {
        let position = self.new_scale;
        std::mem::swap(&mut self.new_scale, &mut self.old_scale);
        position
    }

    fn set_scale(&self, graph: &mut Graph, scale: Vector3<f32>) {
        graph[self.node].local_transform_mut().set_scale(scale);
    }
}

impl<'a> Command<'a> for ScaleNodeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Scale Node".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let scale = self.swap();
        self.set_scale(&mut context.scene.graph, scale);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let scale = self.swap();
        self.set_scale(&mut context.scene.graph, scale);
    }
}

#[derive(Debug)]
pub struct RotateNodeCommand {
    node: Handle<Node>,
    old_rotation: UnitQuaternion<f32>,
    new_rotation: UnitQuaternion<f32>,
}

impl RotateNodeCommand {
    pub fn new(
        node: Handle<Node>,
        old_rotation: UnitQuaternion<f32>,
        new_rotation: UnitQuaternion<f32>,
    ) -> Self {
        Self {
            node,
            old_rotation,
            new_rotation,
        }
    }

    fn swap(&mut self) -> UnitQuaternion<f32> {
        let position = self.new_rotation;
        std::mem::swap(&mut self.new_rotation, &mut self.old_rotation);
        position
    }

    fn set_rotation(
        &self,
        graph: &mut Graph,
        physics: &mut Physics,
        rotation: UnitQuaternion<f32>,
    ) {
        graph[self.node]
            .local_transform_mut()
            .set_rotation(rotation);
        if let Some(&body) = physics.binder.value_of(&self.node) {
            physics.bodies[body].rotation = rotation;
        }
    }
}

impl<'a> Command<'a> for RotateNodeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Rotate Node".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let rotation = self.swap();
        self.set_rotation(
            &mut context.scene.graph,
            &mut context.editor_scene.physics,
            rotation,
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let rotation = self.swap();
        self.set_rotation(
            &mut context.scene.graph,
            &mut context.editor_scene.physics,
            rotation,
        );
    }
}

#[derive(Debug)]
pub struct LinkNodesCommand {
    child: Handle<Node>,
    parent: Handle<Node>,
}

impl LinkNodesCommand {
    pub fn new(child: Handle<Node>, parent: Handle<Node>) -> Self {
        Self { child, parent }
    }

    fn link(&mut self, graph: &mut Graph) {
        let old_parent = graph[self.child].parent();
        graph.link_nodes(self.child, self.parent);
        self.parent = old_parent;
    }
}

impl<'a> Command<'a> for LinkNodesCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Link Nodes".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.link(&mut context.scene.graph);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.link(&mut context.scene.graph);
    }
}

#[derive(Debug)]
pub struct DeleteNodeCommand {
    handle: Handle<Node>,
    ticket: Option<Ticket<Node>>,
    node: Option<Node>,
    parent: Handle<Node>,
}

impl DeleteNodeCommand {
    pub fn new(handle: Handle<Node>) -> Self {
        Self {
            handle,
            ticket: None,
            node: None,
            parent: Default::default(),
        }
    }
}

impl<'a> Command<'a> for DeleteNodeCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Node".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.parent = context.scene.graph[self.handle].parent();
        let (ticket, node) = context.scene.graph.take_reserve(self.handle);
        self.node = Some(node);
        self.ticket = Some(ticket);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.handle = context
            .scene
            .graph
            .put_back(self.ticket.take().unwrap(), self.node.take().unwrap());
        context.scene.graph.link_nodes(self.handle, self.parent);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.scene.graph.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct SetBodyCommand {
    node: Handle<Node>,
    ticket: Option<Ticket<RigidBody>>,
    handle: Handle<RigidBody>,
    body: Option<RigidBody>,
}

impl SetBodyCommand {
    pub fn new(node: Handle<Node>, body: RigidBody) -> Self {
        Self {
            node,
            ticket: None,
            handle: Default::default(),
            body: Some(body),
        }
    }
}

impl<'a> Command<'a> for SetBodyCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Set Node Body".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match self.ticket.take() {
            None => {
                self.handle = context
                    .editor_scene
                    .physics
                    .bodies
                    .spawn(self.body.take().unwrap());
            }
            Some(ticket) => {
                context
                    .editor_scene
                    .physics
                    .bodies
                    .put_back(ticket, self.body.take().unwrap());
            }
        }
        context
            .editor_scene
            .physics
            .binder
            .insert(self.node, self.handle);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context
            .editor_scene
            .physics
            .bodies
            .take_reserve(self.handle);
        self.ticket = Some(ticket);
        self.body = Some(node);
        context
            .editor_scene
            .physics
            .binder
            .remove_by_key(&self.node);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.bodies.forget_ticket(ticket);
            context
                .editor_scene
                .physics
                .binder
                .remove_by_key(&self.node);
        }
    }
}

#[derive(Debug)]
pub struct SetColliderCommand {
    body: Handle<RigidBody>,
    ticket: Option<Ticket<Collider>>,
    handle: Handle<Collider>,
    collider: Option<Collider>,
}

impl SetColliderCommand {
    pub fn new(body: Handle<RigidBody>, collider: Collider) -> Self {
        Self {
            body,
            ticket: None,
            handle: Default::default(),
            collider: Some(collider),
        }
    }
}

impl<'a> Command<'a> for SetColliderCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Set Collider".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        match self.ticket.take() {
            None => {
                self.handle = context
                    .editor_scene
                    .physics
                    .colliders
                    .spawn(self.collider.take().unwrap());
            }
            Some(ticket) => {
                context
                    .editor_scene
                    .physics
                    .colliders
                    .put_back(ticket, self.collider.take().unwrap());
            }
        }
        context.editor_scene.physics.colliders[self.handle].parent = self.body.into();
        context.editor_scene.physics.bodies[self.body]
            .colliders
            .push(self.handle.into());
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let (ticket, mut collider) = context
            .editor_scene
            .physics
            .colliders
            .take_reserve(self.handle);
        collider.parent = Default::default();
        self.ticket = Some(ticket);
        self.collider = Some(collider);

        let body = &mut context.editor_scene.physics.bodies[self.body];
        body.colliders.remove(
            body.colliders
                .iter()
                .position(|&c| c == ErasedHandle::from(self.handle))
                .unwrap(),
        );
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.colliders.forget_ticket(ticket);
        }
    }
}

#[derive(Debug)]
pub struct LoadModelCommand {
    path: PathBuf,
    model: Handle<Node>,
    animations: Vec<Handle<Animation>>,
    sub_graph: Option<SubGraph>,
    animations_container: Vec<(Ticket<Animation>, Animation)>,
}

impl LoadModelCommand {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            model: Default::default(),
            animations: Default::default(),
            sub_graph: None,
            animations_container: Default::default(),
        }
    }
}

impl<'a> Command<'a> for LoadModelCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Load Model".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        if self.model.is_none() {
            // No model was loaded yet, do it.
            if let Ok(model) = rg3d::core::futures::executor::block_on(
                context.resource_manager.request_model(&self.path),
            ) {
                let instance = model.instantiate(context.scene);
                self.model = instance.root;
                self.animations = instance.animations;

                // Enable instantiated animations.
                for &animation in self.animations.iter() {
                    context.scene.animations[animation].set_enabled(true);
                }
            }
        } else {
            // A model was loaded, but change was reverted and here we must put all nodes
            // back to graph.
            self.model = context
                .scene
                .graph
                .put_sub_graph_back(self.sub_graph.take().unwrap());
            for (ticket, animation) in self.animations_container.drain(..) {
                context.scene.animations.put_back(ticket, animation);
            }
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.sub_graph = Some(context.scene.graph.take_reserve_sub_graph(self.model));
        self.animations_container = self
            .animations
            .iter()
            .map(|&anim| context.scene.animations.take_reserve(anim))
            .collect();
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(sub_graph) = self.sub_graph.take() {
            context.scene.graph.forget_sub_graph(sub_graph)
        }
        for (ticket, _) in self.animations_container.drain(..) {
            context.scene.animations.forget_ticket(ticket);
        }
    }
}

#[derive(Debug)]
pub struct DeleteSubGraphCommand {
    sub_graph_root: Handle<Node>,
    sub_graph: Option<SubGraph>,
    parent: Handle<Node>,
}

impl DeleteSubGraphCommand {
    pub fn new(sub_graph_root: Handle<Node>) -> Self {
        Self {
            sub_graph_root,
            sub_graph: None,
            parent: Handle::NONE,
        }
    }
}

impl<'a> Command<'a> for DeleteSubGraphCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Sub Graph".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.parent = context.scene.graph[self.sub_graph_root].parent();
        self.sub_graph = Some(
            context
                .scene
                .graph
                .take_reserve_sub_graph(self.sub_graph_root),
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        context
            .scene
            .graph
            .put_sub_graph_back(self.sub_graph.take().unwrap());
        context
            .scene
            .graph
            .link_nodes(self.sub_graph_root, self.parent);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(sub_graph) = self.sub_graph.take() {
            context.scene.graph.forget_sub_graph(sub_graph)
        }
    }
}

#[derive(Debug)]
pub struct DeleteBodyCommand {
    handle: Handle<RigidBody>,
    ticket: Option<Ticket<RigidBody>>,
    body: Option<RigidBody>,
    node: Handle<Node>,
}

impl DeleteBodyCommand {
    pub fn new(handle: Handle<RigidBody>) -> Self {
        Self {
            handle,
            ticket: None,
            body: None,
            node: Handle::NONE,
        }
    }
}

impl<'a> Command<'a> for DeleteBodyCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Body".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let (ticket, node) = context
            .editor_scene
            .physics
            .bodies
            .take_reserve(self.handle);
        self.body = Some(node);
        self.ticket = Some(ticket);
        self.node = context.editor_scene.physics.unbind_by_body(self.handle);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.handle = context
            .editor_scene
            .physics
            .bodies
            .put_back(self.ticket.take().unwrap(), self.body.take().unwrap());
        context
            .editor_scene
            .physics
            .binder
            .insert(self.node, self.handle);
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.bodies.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct DeleteColliderCommand {
    handle: Handle<Collider>,
    ticket: Option<Ticket<Collider>>,
    collider: Option<Collider>,
    body: Handle<RigidBody>,
}

impl DeleteColliderCommand {
    pub fn new(handle: Handle<Collider>) -> Self {
        Self {
            handle,
            ticket: None,
            collider: None,
            body: Handle::NONE,
        }
    }
}

impl<'a> Command<'a> for DeleteColliderCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Delete Collider".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let (ticket, collider) = context
            .editor_scene
            .physics
            .colliders
            .take_reserve(self.handle);
        self.body = collider.parent.into();
        self.collider = Some(collider);
        self.ticket = Some(ticket);

        let body = &mut context.editor_scene.physics.bodies[self.body];
        body.colliders.remove(
            body.colliders
                .iter()
                .position(|&c| c == ErasedHandle::from(self.handle))
                .unwrap(),
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.handle = context
            .editor_scene
            .physics
            .colliders
            .put_back(self.ticket.take().unwrap(), self.collider.take().unwrap());

        let body = &mut context.editor_scene.physics.bodies[self.body];
        body.colliders.push(self.handle.into());
    }

    fn finalize(&mut self, context: &mut Self::Context) {
        if let Some(ticket) = self.ticket.take() {
            context.editor_scene.physics.colliders.forget_ticket(ticket)
        }
    }
}

#[derive(Debug)]
pub struct AddLodGroupLevelCommand {
    handle: Handle<Node>,
    level: LevelOfDetail,
}

impl AddLodGroupLevelCommand {
    pub fn new(handle: Handle<Node>, level: LevelOfDetail) -> Self {
        Self { handle, level }
    }
}

impl<'a> Command<'a> for AddLodGroupLevelCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Lod Group Level".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels
            .push(self.level.clone());
    }

    fn revert(&mut self, context: &mut Self::Context) {
        context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels
            .pop();
    }
}

#[derive(Debug)]
pub struct RemoveLodGroupLevelCommand {
    handle: Handle<Node>,
    level: Option<LevelOfDetail>,
    index: usize,
}

impl RemoveLodGroupLevelCommand {
    pub fn new(handle: Handle<Node>, index: usize) -> Self {
        Self {
            handle,
            level: None,
            index,
        }
    }
}

impl<'a> Command<'a> for RemoveLodGroupLevelCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Remove Lod Group Level".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.level = Some(
            context.scene.graph[self.handle]
                .lod_group_mut()
                .unwrap()
                .levels
                .remove(self.index),
        );
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let group = context.scene.graph[self.handle].lod_group_mut().unwrap();
        let level = self.level.take().unwrap();
        if group.levels.is_empty() {
            group.levels.push(level);
        } else {
            group.levels.insert(self.index, level)
        }
    }
}

#[derive(Debug)]
pub struct AddLodObjectCommand {
    handle: Handle<Node>,
    lod_index: usize,
    object: Handle<Node>,
    object_index: usize,
}

impl AddLodObjectCommand {
    pub fn new(handle: Handle<Node>, lod_index: usize, object: Handle<Node>) -> Self {
        Self {
            handle,
            lod_index,
            object,
            object_index: 0,
        }
    }
}

impl<'a> Command<'a> for AddLodObjectCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Add Lod Object".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        let objects = &mut context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index]
            .objects;
        self.object_index = objects.len();
        objects.push(self.object);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index]
            .objects
            .remove(self.object_index);
    }
}

#[derive(Debug)]
pub struct RemoveLodObjectCommand {
    handle: Handle<Node>,
    lod_index: usize,
    object: Handle<Node>,
    object_index: usize,
}

impl RemoveLodObjectCommand {
    pub fn new(handle: Handle<Node>, lod_index: usize, object_index: usize) -> Self {
        Self {
            handle,
            lod_index,
            object: Default::default(),
            object_index,
        }
    }
}

impl<'a> Command<'a> for RemoveLodObjectCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Remove Lod Object".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.object = context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index]
            .objects
            .remove(self.object_index);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        let objects = &mut context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index]
            .objects;
        if objects.is_empty() {
            objects.push(self.object);
        } else {
            objects.insert(self.object_index, self.object);
        }
    }
}

#[derive(Debug)]
pub struct ChangeLodRangeBeginCommand {
    handle: Handle<Node>,
    lod_index: usize,
    new_value: f32,
}

impl ChangeLodRangeBeginCommand {
    pub fn new(handle: Handle<Node>, lod_index: usize, new_value: f32) -> Self {
        Self {
            handle,
            lod_index,
            new_value,
        }
    }

    fn swap(&mut self, context: &mut SceneContext) {
        let level = &mut context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index];
        let old = level.begin();
        level.set_begin(self.new_value);
        self.new_value = old;
    }
}

impl<'a> Command<'a> for ChangeLodRangeBeginCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Change Lod Range Begin".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }
}

#[derive(Debug)]
pub struct ChangeLodRangeEndCommand {
    handle: Handle<Node>,
    lod_index: usize,
    new_value: f32,
}

impl ChangeLodRangeEndCommand {
    pub fn new(handle: Handle<Node>, lod_index: usize, new_value: f32) -> Self {
        Self {
            handle,
            lod_index,
            new_value,
        }
    }

    fn swap(&mut self, context: &mut SceneContext) {
        let level = &mut context.scene.graph[self.handle]
            .lod_group_mut()
            .unwrap()
            .levels[self.lod_index];
        let old = level.end();
        level.set_end(self.new_value);
        self.new_value = old;
    }
}

impl<'a> Command<'a> for ChangeLodRangeEndCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Change Lod Range End".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }
}

#[derive(Debug)]
enum TextureSet {
    Single(Texture),
    Multiple(Vec<Option<Texture>>),
}

#[derive(Debug)]
pub struct SetMeshTextureCommand {
    node: Handle<Node>,
    set: TextureSet,
}

impl SetMeshTextureCommand {
    pub fn new(node: Handle<Node>, texture: Texture) -> Self {
        Self {
            node,
            set: TextureSet::Single(texture),
        }
    }
}

impl<'a> Command<'a> for SetMeshTextureCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        "Set Texture".to_owned()
    }

    fn execute(&mut self, context: &mut Self::Context) {
        if let TextureSet::Single(texture) = &self.set {
            let mesh: &mut Mesh = context.scene.graph[self.node].as_mesh_mut();
            let old_set = mesh
                .surfaces_mut()
                .iter()
                .map(|s| s.diffuse_texture())
                .collect();
            for surface in mesh.surfaces_mut() {
                surface.set_diffuse_texture(Some(texture.clone()));
            }
            self.set = TextureSet::Multiple(old_set);
        } else {
            unreachable!()
        }
    }

    fn revert(&mut self, context: &mut Self::Context) {
        if let TextureSet::Multiple(set) = &self.set {
            let mesh: &mut Mesh = context.scene.graph[self.node].as_mesh_mut();
            let new_value = mesh.surfaces_mut()[0].diffuse_texture().unwrap();
            assert_eq!(mesh.surfaces_mut().len(), set.len());
            for (surface, old_texture) in mesh.surfaces_mut().iter_mut().zip(set) {
                surface.set_diffuse_texture(old_texture.clone());
            }
            self.set = TextureSet::Single(new_value);
        } else {
            unreachable!()
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum EmitterNumericParameter {
    SpawnRate,
    MaxParticles,
    MinLifetime,
    MaxLifetime,
    MinSizeModifier,
    MaxSizeModifier,
    MinXVelocity,
    MaxXVelocity,
    MinYVelocity,
    MaxYVelocity,
    MinZVelocity,
    MaxZVelocity,
    MinRotationSpeed,
    MaxRotationSpeed,
    MinRotation,
    MaxRotation,
}

impl EmitterNumericParameter {
    fn name(self) -> &'static str {
        match self {
            EmitterNumericParameter::SpawnRate => "SpawnRate",
            EmitterNumericParameter::MaxParticles => "MaxParticles",
            EmitterNumericParameter::MinLifetime => "MinLifetime",
            EmitterNumericParameter::MaxLifetime => "MaxLifetime",
            EmitterNumericParameter::MinSizeModifier => "MinSizeModifier",
            EmitterNumericParameter::MaxSizeModifier => "MaxSizeModifier",
            EmitterNumericParameter::MinXVelocity => "MinXVelocity",
            EmitterNumericParameter::MaxXVelocity => "MaxXVelocity",
            EmitterNumericParameter::MinYVelocity => "MinYVelocity",
            EmitterNumericParameter::MaxYVelocity => "MaxYVelocity",
            EmitterNumericParameter::MinZVelocity => "MinZVelocity",
            EmitterNumericParameter::MaxZVelocity => "MaxZVelocity",
            EmitterNumericParameter::MinRotationSpeed => "MinRotationSpeed",
            EmitterNumericParameter::MaxRotationSpeed => "MaxRotationSpeed",
            EmitterNumericParameter::MinRotation => "MinRotation",
            EmitterNumericParameter::MaxRotation => "MaxRotation",
        }
    }
}

#[derive(Debug)]
pub struct SetEmitterNumericParameterCommand {
    node: Handle<Node>,
    parameter: EmitterNumericParameter,
    value: f32,
    emitter_index: usize,
}

impl SetEmitterNumericParameterCommand {
    pub fn new(
        node: Handle<Node>,
        emitter_index: usize,
        parameter: EmitterNumericParameter,
        value: f32,
    ) -> Self {
        Self {
            node,
            parameter,
            value,
            emitter_index,
        }
    }

    fn swap(&mut self, context: &mut SceneContext) {
        let emitter: &mut Emitter = &mut context.scene.graph[self.node]
            .as_particle_system_mut()
            .emitters[self.emitter_index];
        match self.parameter {
            EmitterNumericParameter::SpawnRate => {
                let old = emitter.spawn_rate();
                emitter.set_spawn_rate(self.value as u32);
                self.value = old as f32;
            }
            EmitterNumericParameter::MaxParticles => {
                let old = emitter.max_particles();
                emitter.set_max_particles(if self.value < 0.0 {
                    ParticleLimit::Unlimited
                } else {
                    ParticleLimit::Strict(self.value as u32)
                });
                self.value = match old {
                    ParticleLimit::Unlimited => -1.0,
                    ParticleLimit::Strict(value) => value as f32,
                };
            }
            EmitterNumericParameter::MinLifetime => {
                let old = emitter.life_time_range();
                emitter.set_life_time_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxLifetime => {
                let old = emitter.life_time_range();
                emitter.set_life_time_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinSizeModifier => {
                let old = emitter.size_modifier_range();
                emitter.set_size_modifier_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxSizeModifier => {
                let old = emitter.size_modifier_range();
                emitter.set_size_modifier_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinXVelocity => {
                let old = emitter.x_velocity_range();
                emitter.set_x_velocity_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxXVelocity => {
                let old = emitter.x_velocity_range();
                emitter.set_x_velocity_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinYVelocity => {
                let old = emitter.y_velocity_range();
                emitter.set_y_velocity_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxYVelocity => {
                let old = emitter.y_velocity_range();
                emitter.set_y_velocity_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinZVelocity => {
                let old = emitter.z_velocity_range();
                emitter.set_z_velocity_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxZVelocity => {
                let old = emitter.z_velocity_range();
                emitter.set_z_velocity_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinRotationSpeed => {
                let old = emitter.rotation_speed_range();
                emitter.set_rotation_speed_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxRotationSpeed => {
                let old = emitter.rotation_speed_range();
                emitter.set_rotation_speed_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
            EmitterNumericParameter::MinRotation => {
                let old = emitter.rotation_range();
                emitter.set_rotation_range(NumericRange::new(self.value, old.bounds[1]));
                self.value = old.bounds[0];
            }
            EmitterNumericParameter::MaxRotation => {
                let old = emitter.rotation_range();
                emitter.set_rotation_range(NumericRange::new(old.bounds[0], self.value));
                self.value = old.bounds[1];
            }
        };
    }
}

impl<'a> Command<'a> for SetEmitterNumericParameterCommand {
    type Context = SceneContext<'a>;

    fn name(&mut self, _context: &Self::Context) -> String {
        format!("Set Emitter F32 Parameter: {}", self.parameter.name())
    }

    fn execute(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }

    fn revert(&mut self, context: &mut Self::Context) {
        self.swap(context);
    }
}

macro_rules! define_node_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $node:ident) $apply_method:block ) => {
        #[derive(Debug)]
        pub struct $name {
            handle: Handle<Node>,
            value: $value_type,
        }

        impl $name {
            pub fn new(handle: Handle<Node>, value: $value_type) -> Self {
                Self { handle, value }
            }

            fn swap(&mut $self, graph: &mut Graph) {
                let $node = &mut graph[$self.handle];
                $apply_method
            }
        }

        impl<'a> Command<'a> for $name {
            type Context = SceneContext<'a>;

            fn name(&mut self, _context: &Self::Context) -> String {
                $human_readable_name.to_owned()
            }

            fn execute(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.scene.graph);
            }

            fn revert(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.scene.graph);
            }
        }
    };
}

macro_rules! define_physics_command {
    ($name:ident($human_readable_name:expr, $handle_type:ty, $value_type:ty) where fn swap($self:ident, $physics:ident) $apply_method:block ) => {
        #[derive(Debug)]
        pub struct $name {
            handle: Handle<$handle_type>,
            value: $value_type,
        }

        impl $name {
            pub fn new(handle: Handle<$handle_type>, value: $value_type) -> Self {
                Self { handle, value }
            }

            fn swap(&mut $self, $physics: &mut Physics) {
                 $apply_method
            }
        }

        impl<'a> Command<'a> for $name {
            type Context = SceneContext<'a>;

            fn name(&mut self, _context: &Self::Context) -> String {
                $human_readable_name.to_owned()
            }

            fn execute(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.editor_scene.physics);
            }

            fn revert(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.editor_scene.physics);
            }
        }
    };
}

macro_rules! define_body_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $physics: ident, $body:ident) $apply_method:block ) => {
        define_physics_command!($name($human_readable_name, RigidBody, $value_type) where fn swap($self, $physics) {
            let $body = &mut $physics.bodies[$self.handle];
            $apply_method
        });
    };
}

macro_rules! define_collider_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $physics:ident, $collider:ident) $apply_method:block ) => {
        define_physics_command!($name($human_readable_name, Collider, $value_type) where fn swap($self, $physics) {
            let $collider = &mut $physics.colliders[$self.handle];
            $apply_method
        });
    };
}

macro_rules! define_joint_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $physics:ident, $joint:ident) $apply_method:block ) => {
        define_physics_command!($name($human_readable_name, Joint, $value_type) where fn swap($self, $physics) {
            let $joint = &mut $physics.joints[$self.handle];
            $apply_method
        });
    };
}

macro_rules! define_joint_variant_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $physics:ident, $variant:ident, $var:ident) $apply_method:block ) => {
        define_physics_command!($name($human_readable_name, Joint, $value_type) where fn swap($self, $physics) {
            let joint = &mut $physics.joints[$self.handle];
            if let JointParamsDesc::$variant($var) = &mut joint.params {
                $apply_method
            } else {
                unreachable!();
            }
        });
    };
}

macro_rules! define_collider_variant_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $physics:ident, $variant:ident, $var:ident) $apply_method:block ) => {
        define_physics_command!($name($human_readable_name, Collider, $value_type) where fn swap($self, $physics) {
            let collider = &mut $physics.colliders[$self.handle];
            if let ColliderShapeDesc::$variant($var) = &mut collider.shape {
                $apply_method
            } else {
                unreachable!();
            }
        });
    };
}

macro_rules! define_emitter_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $emitter:ident) $apply_method:block ) => {
        #[derive(Debug)]
        pub struct $name {
            handle: Handle<Node>,
            value: $value_type,
            index: usize
        }

        impl $name {
            pub fn new(handle: Handle<Node>, index: usize, value: $value_type) -> Self {
                Self { handle, index, value }
            }

            fn swap(&mut $self, graph: &mut Graph) {
                let $emitter = &mut graph[$self.handle].as_particle_system_mut().emitters[$self.index];
                $apply_method
            }
        }

        impl<'a> Command<'a> for $name {
            type Context = SceneContext<'a>;

            fn name(&mut self, _context: &Self::Context) -> String {
                $human_readable_name.to_owned()
            }

            fn execute(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.scene.graph);
            }

            fn revert(&mut self, context: &mut Self::Context) {
                self.swap(&mut context.scene.graph);
            }
        }
    };
}

macro_rules! define_emitter_variant_command {
    ($name:ident($human_readable_name:expr, $value_type:ty) where fn swap($self:ident, $emitter:ident, $variant:ident, $var:ident) $apply_method:block ) => {
        define_emitter_command!($name($human_readable_name, $value_type) where fn swap($self, $emitter) {
            if let Emitter::$variant($var) = $emitter {
                $apply_method
            } else {
                unreachable!()
            }
        });
    };
}

macro_rules! get_set_swap {
    ($self:ident, $host:expr, $get:ident, $set:ident) => {
        match $host {
            host => {
                let old = host.$get();
                host.$set($self.value.clone());
                $self.value = old;
            }
        }
    };
}

define_node_command!(SetLightScatterCommand("Set Light Scatter", Vector3<f32>) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut(), scatter, set_scatter)
});

define_node_command!(SetLightScatterEnabledCommand("Set Light Scatter Enabled", bool) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut(), is_scatter_enabled, enable_scatter)
});

define_node_command!(SetLightCastShadowsCommand("Set Light Cast Shadows", bool) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut(), is_cast_shadows, set_cast_shadows)
});

define_node_command!(SetPointLightRadiusCommand("Set Point Light Radius", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut().as_point_mut(), radius, set_radius)
});

define_node_command!(SetSpotLightHotspotCommand("Set Spot Light Hotspot", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut().as_spot_mut(), hotspot_cone_angle, set_hotspot_cone_angle)
});

define_node_command!(SetSpotLightFalloffAngleDeltaCommand("Set Spot Light Falloff Angle Delta", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut().as_spot_mut(), falloff_angle_delta, set_falloff_angle_delta)
});

define_node_command!(SetSpotLightDistanceCommand("Set Spot Light Distance", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut().as_spot_mut(), distance, set_distance);
});

define_node_command!(SetLightColorCommand("Set Light Color", Color) where fn swap(self, node) {
    get_set_swap!(self, node.as_light_mut(), color, set_color)
});

define_node_command!(SetNameCommand("Set Name", String) where fn swap(self, node) {
    get_set_swap!(self, node, name_owned, set_name);
});

define_node_command!(SetLodGroupCommand("Set Lod Group", Option<LodGroup>) where fn swap(self, node) {
    get_set_swap!(self, node, take_lod_group, set_lod_group);
});

define_node_command!(SetPhysicsBindingCommand("Set Physics Binding", PhysicsBinding) where fn swap(self, node) {
    get_set_swap!(self, node, physics_binding, set_physics_binding);
});

define_node_command!(SetTagCommand("Set Tag", String) where fn swap(self, node) {
    get_set_swap!(self, node, tag_owned, set_tag);
});

define_node_command!(SetVisibleCommand("Set Visible", bool) where fn swap(self, node) {
    get_set_swap!(self, node, visibility, set_visibility)
});

define_node_command!(SetFovCommand("Set Fov", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_camera_mut(), fov, set_fov);
});

define_node_command!(SetZNearCommand("Set Camera Z Near", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_camera_mut(), z_near, set_z_near);
});

define_node_command!(SetZFarCommand("Set Camera Z Far", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_camera_mut(), z_far, set_z_far);
});

define_node_command!(SetParticleSystemAccelerationCommand("Set Particle System Acceleration", Vector3<f32>) where fn swap(self, node) {
    get_set_swap!(self, node.as_particle_system_mut(), acceleration, set_acceleration);
});

define_node_command!(SetSpriteSizeCommand("Set Sprite Size", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_sprite_mut(), size, set_size);
});

define_node_command!(SetSpriteRotationCommand("Set Sprite Rotation", f32) where fn swap(self, node) {
    get_set_swap!(self, node.as_sprite_mut(), rotation, set_rotation);
});

define_node_command!(SetSpriteColorCommand("Set Sprite Color", Color) where fn swap(self, node) {
    get_set_swap!(self, node.as_sprite_mut(), color, set_color);
});

define_node_command!(SetSpriteTextureCommand("Set Sprite Texture", Option<Texture>) where fn swap(self, node) {
    get_set_swap!(self, node.as_sprite_mut(), texture, set_texture);
});

define_node_command!(SetParticleSystemTextureCommand("Set Particle System Texture", Option<Texture>) where fn swap(self, node) {
    get_set_swap!(self, node.as_particle_system_mut(), texture, set_texture);
});

define_node_command!(SetMeshCastShadowsCommand("Set Mesh Cast Shadows", bool) where fn swap(self, node) {
    get_set_swap!(self, node.as_mesh_mut(), cast_shadows, set_cast_shadows);
});

define_node_command!(SetMeshRenderPathCommand("Set Mesh Render Path", RenderPath) where fn swap(self, node) {
    get_set_swap!(self, node.as_mesh_mut(), render_path, set_render_path);
});

define_body_command!(SetBodyMassCommand("Set Body Mass", f32) where fn swap(self, physics, body) {
    std::mem::swap(&mut body.mass, &mut self.value);
});

define_collider_command!(SetColliderFrictionCommand("Set Collider Friction", f32) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.friction, &mut self.value);
});

define_collider_command!(SetColliderRestitutionCommand("Set Collider Restitution", f32) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.restitution, &mut self.value);
});

define_collider_command!(SetColliderPositionCommand("Set Collider Position", Vector3<f32>) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.translation, &mut self.value);
});

define_collider_command!(SetColliderRotationCommand("Set Collider Rotation", UnitQuaternion<f32>) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.rotation, &mut self.value);
});

define_collider_command!(SetColliderIsSensorCommand("Set Collider Is Sensor", bool) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.is_sensor, &mut self.value);
});

define_collider_command!(SetColliderCollisionGroupsCommand("Set Collider Collision Groups", u32) where fn swap(self, physics, collider) {
    std::mem::swap(&mut collider.collision_groups, &mut self.value);
});

define_collider_variant_command!(SetCylinderHalfHeightCommand("Set Cylinder Half Height", f32) where fn swap(self, physics, Cylinder, cylinder) {
    std::mem::swap(&mut cylinder.half_height, &mut self.value);
});

define_collider_variant_command!(SetCylinderRadiusCommand("Set Cylinder Radius", f32) where fn swap(self, physics, Cylinder, cylinder) {
    std::mem::swap(&mut cylinder.radius, &mut self.value);
});

define_collider_variant_command!(SetConeHalfHeightCommand("Set Cone Half Height", f32) where fn swap(self, physics, Cone, cone) {
    std::mem::swap(&mut cone.half_height, &mut self.value);
});

define_collider_variant_command!(SetConeRadiusCommand("Set Cone Radius", f32) where fn swap(self, physics, Cone, cone) {
    std::mem::swap(&mut cone.radius, &mut self.value);
});

define_collider_variant_command!(SetCuboidHalfExtentsCommand("Set Cuboid Half Extents", Vector3<f32>) where fn swap(self, physics, Cuboid, cuboid) {
    std::mem::swap(&mut cuboid.half_extents, &mut self.value);
});

define_collider_variant_command!(SetCapsuleRadiusCommand("Set Capsule Radius", f32) where fn swap(self, physics, Capsule, capsule) {
    std::mem::swap(&mut capsule.radius, &mut self.value);
});

define_collider_variant_command!(SetCapsuleBeginCommand("Set Capsule Begin", Vector3<f32>) where fn swap(self, physics, Capsule, capsule) {
    std::mem::swap(&mut capsule.begin, &mut self.value);
});

define_collider_variant_command!(SetCapsuleEndCommand("Set Capsule End", Vector3<f32>) where fn swap(self, physics, Capsule, capsule) {
    std::mem::swap(&mut capsule.end, &mut self.value);
});

define_collider_variant_command!(SetBallRadiusCommand("Set Ball Radius", f32) where fn swap(self, physics, Ball, ball) {
    std::mem::swap(&mut ball.radius, &mut self.value);
});

define_joint_variant_command!(SetBallJointAnchor1Command("Set Ball Joint Anchor 1", Vector3<f32>) where fn swap(self, physics, BallJoint, ball) {
    std::mem::swap(&mut ball.local_anchor1, &mut self.value);
});

define_joint_variant_command!(SetBallJointAnchor2Command("Set Ball Joint Anchor 2", Vector3<f32>) where fn swap(self, physics, BallJoint, ball) {
    std::mem::swap(&mut ball.local_anchor2, &mut self.value);
});

define_joint_variant_command!(SetFixedJointAnchor1TranslationCommand("Set Fixed Joint Anchor 1 Translation", Vector3<f32>) where fn swap(self, physics, FixedJoint, fixed) {
    std::mem::swap(&mut fixed.local_anchor1_translation, &mut self.value);
});

define_joint_variant_command!(SetFixedJointAnchor2TranslationCommand("Set Fixed Joint Anchor 2 Translation", Vector3<f32>) where fn swap(self, physics, FixedJoint, fixed) {
    std::mem::swap(&mut fixed.local_anchor2_translation, &mut self.value);
});

define_joint_variant_command!(SetFixedJointAnchor1RotationCommand("Set Fixed Joint Anchor 1 Rotation", UnitQuaternion<f32>) where fn swap(self, physics, FixedJoint, fixed) {
    std::mem::swap(&mut fixed.local_anchor1_rotation, &mut self.value);
});

define_joint_variant_command!(SetFixedJointAnchor2RotationCommand("Set Fixed Joint Anchor 2 Rotation", UnitQuaternion<f32>) where fn swap(self, physics, FixedJoint, fixed) {
    std::mem::swap(&mut fixed.local_anchor2_rotation, &mut self.value);
});

define_joint_variant_command!(SetRevoluteJointAnchor1Command("Set Revolute Joint Anchor 1", Vector3<f32>) where fn swap(self, physics, RevoluteJoint, revolute) {
    std::mem::swap(&mut revolute.local_anchor1, &mut self.value);
});

define_joint_variant_command!(SetRevoluteJointAxis1Command("Set Revolute Joint Axis 1", Vector3<f32>) where fn swap(self, physics, RevoluteJoint, revolute) {
    std::mem::swap(&mut revolute.local_axis1, &mut self.value);
});

define_joint_variant_command!(SetRevoluteJointAnchor2Command("Set Revolute Joint Anchor 2", Vector3<f32>) where fn swap(self, physics, RevoluteJoint, revolute) {
    std::mem::swap(&mut revolute.local_anchor2, &mut self.value);
});

define_joint_variant_command!(SetRevoluteJointAxis2Command("Set Prismatic Joint Axis 2", Vector3<f32>) where fn swap(self, physics, RevoluteJoint, revolute) {
    std::mem::swap(&mut revolute.local_axis2, &mut self.value);
});

define_joint_variant_command!(SetPrismaticJointAnchor1Command("Set Prismatic Joint Anchor 1", Vector3<f32>) where fn swap(self, physics, PrismaticJoint, prismatic) {
    std::mem::swap(&mut prismatic.local_anchor1, &mut self.value);
});

define_joint_variant_command!(SetPrismaticJointAxis1Command("Set Prismatic Joint Axis 1", Vector3<f32>) where fn swap(self, physics, PrismaticJoint, prismatic) {
    std::mem::swap(&mut prismatic.local_axis1, &mut self.value);
});

define_joint_variant_command!(SetPrismaticJointAnchor2Command("Set Prismatic Joint Anchor 2", Vector3<f32>) where fn swap(self, physics, PrismaticJoint, prismatic) {
    std::mem::swap(&mut prismatic.local_anchor2, &mut self.value);
});

define_joint_variant_command!(SetPrismaticJointAxis2Command("Set Prismatic Joint Axis 2", Vector3<f32>) where fn swap(self, physics, PrismaticJoint, prismatic) {
    std::mem::swap(&mut prismatic.local_axis2, &mut self.value);
});

define_joint_command!(SetJointConnectedBodyCommand("Set Joint Connected Body", ErasedHandle) where fn swap(self, physics, joint) {
    std::mem::swap(&mut joint.body2, &mut self.value);
});

define_emitter_command!(SetEmitterPositionCommand("Set Emitter Position", Vector3<f32>) where fn swap(self, emitter) {
    get_set_swap!(self, emitter, position, set_position);
});

define_emitter_variant_command!(SetSphereEmitterRadiusCommand("Set Sphere Emitter Radius", f32) where fn swap(self, emitter, Sphere, sphere) {
    get_set_swap!(self, sphere, radius, set_radius);
});

define_emitter_variant_command!(SetCylinderEmitterRadiusCommand("Set Cylinder Emitter Radius", f32) where fn swap(self, emitter, Cylinder, cylinder) {
    get_set_swap!(self, cylinder, radius, set_radius);
});

define_emitter_variant_command!(SetCylinderEmitterHeightCommand("Set Cylinder Emitter Radius", f32) where fn swap(self, emitter, Cylinder, cylinder) {
    get_set_swap!(self, cylinder, height, set_height);
});

define_emitter_variant_command!(SetBoxEmitterHalfWidthCommand("Set Box Emitter Half Width", f32) where fn swap(self, emitter, Box, box_emitter) {
    get_set_swap!(self, box_emitter, half_width, set_half_width);
});

define_emitter_variant_command!(SetBoxEmitterHalfHeightCommand("Set Box Emitter Half Height", f32) where fn swap(self, emitter, Box, box_emitter) {
    get_set_swap!(self, box_emitter, half_height, set_half_height);
});

define_emitter_variant_command!(SetBoxEmitterHalfDepthCommand("Set Box Emitter Half Depth", f32) where fn swap(self, emitter, Box, box_emitter) {
    get_set_swap!(self, box_emitter, half_depth, set_half_depth);
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    None,
    Graph(GraphSelection),
    Navmesh(NavmeshSelection),
}

impl Default for Selection {
    fn default() -> Self {
        Self::None
    }
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        match self {
            Selection::None => true,
            Selection::Graph(graph) => graph.is_empty(),
            Selection::Navmesh(navmesh) => navmesh.is_empty(),
        }
    }
}

#[derive(Debug, Default, Clone, Eq)]
pub struct GraphSelection {
    nodes: Vec<Handle<Node>>,
}

impl PartialEq for GraphSelection {
    fn eq(&self, other: &Self) -> bool {
        if self.nodes.is_empty() && !other.nodes.is_empty() {
            false
        } else {
            // Selection is equal even when order of elements is different.
            // TODO: Find a way to do this faster.
            for &node in self.nodes.iter() {
                let mut found = false;
                for &other_node in other.nodes.iter() {
                    if other_node == node {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return false;
                }
            }
            true
        }
    }
}

impl GraphSelection {
    pub fn from_list(nodes: Vec<Handle<Node>>) -> Self {
        Self {
            nodes: nodes.into_iter().filter(|h| h.is_some()).collect(),
        }
    }

    /// Creates new selection as single if node handle is not none, and empty if it is.
    pub fn single_or_empty(node: Handle<Node>) -> Self {
        if node.is_none() {
            Self {
                nodes: Default::default(),
            }
        } else {
            Self { nodes: vec![node] }
        }
    }

    /// Adds new selected node, or removes it if it is already in the list of selected nodes.
    pub fn insert_or_exclude(&mut self, handle: Handle<Node>) {
        if let Some(position) = self.nodes.iter().position(|&h| h == handle) {
            self.nodes.remove(position);
        } else {
            self.nodes.push(handle);
        }
    }

    pub fn contains(&self, handle: Handle<Node>) -> bool {
        self.nodes.iter().any(|&h| h == handle)
    }

    pub fn nodes(&self) -> &[Handle<Node>] {
        &self.nodes
    }

    pub fn is_multi_selection(&self) -> bool {
        self.nodes.len() > 1
    }

    pub fn is_single_selection(&self) -> bool {
        self.nodes.len() == 1
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn extend(&mut self, other: &GraphSelection) {
        self.nodes.extend_from_slice(&other.nodes)
    }

    pub fn root_nodes(&self, graph: &Graph) -> Vec<Handle<Node>> {
        // Helper function.
        fn is_descendant_of(handle: Handle<Node>, other: Handle<Node>, graph: &Graph) -> bool {
            for &child in graph[other].children() {
                if child == handle {
                    return true;
                }

                let inner = is_descendant_of(handle, child, graph);
                if inner {
                    return true;
                }
            }
            false
        }

        let mut root_nodes = Vec::new();
        for &node in self.nodes().iter() {
            let mut descendant = false;
            for &other_node in self.nodes().iter() {
                if is_descendant_of(node, other_node, graph) {
                    descendant = true;
                    break;
                }
            }
            if !descendant {
                root_nodes.push(node);
            }
        }
        root_nodes
    }

    pub fn global_rotation_position(
        &self,
        graph: &Graph,
    ) -> Option<(UnitQuaternion<f32>, Vector3<f32>)> {
        if self.is_single_selection() {
            Some(graph.global_rotation_position_no_scale(self.nodes[0]))
        } else if self.is_empty() {
            None
        } else {
            let mut position = Vector3::default();
            let mut rotation = graph.global_rotation(self.nodes[0]);
            let t = 1.0 / self.nodes.len() as f32;
            for &handle in self.nodes.iter() {
                let global_transform = graph[handle].global_transform();
                position += global_transform.position();
                rotation = rotation.slerp(&graph.global_rotation(self.nodes[0]), t);
            }
            position = position.scale(t);
            Some((rotation, position))
        }
    }

    pub fn offset(&self, graph: &mut Graph, offset: Vector3<f32>) {
        for &handle in self.nodes.iter() {
            let mut chain_scale = Vector3::new(1.0, 1.0, 1.0);
            let mut parent_handle = graph[handle].parent();
            while parent_handle.is_some() {
                let parent = &graph[parent_handle];
                let parent_scale = parent.local_transform().scale();
                chain_scale.x *= parent_scale.x;
                chain_scale.y *= parent_scale.y;
                chain_scale.z *= parent_scale.z;
                parent_handle = parent.parent();
            }

            let offset = Vector3::new(
                if chain_scale.x.abs() > 0.0 {
                    offset.x / chain_scale.x
                } else {
                    offset.x
                },
                if chain_scale.y.abs() > 0.0 {
                    offset.y / chain_scale.y
                } else {
                    offset.y
                },
                if chain_scale.z.abs() > 0.0 {
                    offset.z / chain_scale.z
                } else {
                    offset.z
                },
            );
            graph[handle].local_transform_mut().offset(offset);
        }
    }

    pub fn rotate(&self, graph: &mut Graph, rotation: UnitQuaternion<f32>) {
        for &handle in self.nodes.iter() {
            graph[handle].local_transform_mut().set_rotation(rotation);
        }
    }

    pub fn scale(&self, graph: &mut Graph, scale: Vector3<f32>) {
        for &handle in self.nodes.iter() {
            graph[handle].local_transform_mut().set_scale(scale);
        }
    }

    pub fn local_positions(&self, graph: &Graph) -> Vec<Vector3<f32>> {
        let mut positions = Vec::new();
        for &handle in self.nodes.iter() {
            positions.push(**graph[handle].local_transform().position());
        }
        positions
    }

    pub fn local_rotations(&self, graph: &Graph) -> Vec<UnitQuaternion<f32>> {
        let mut rotations = Vec::new();
        for &handle in self.nodes.iter() {
            rotations.push(**graph[handle].local_transform().rotation());
        }
        rotations
    }

    pub fn local_scales(&self, graph: &Graph) -> Vec<Vector3<f32>> {
        let mut scales = Vec::new();
        for &handle in self.nodes.iter() {
            scales.push(**graph[handle].local_transform().scale());
        }
        scales
    }
}

/// Creates scene command (command group) which removes current selection in editor's scene.
/// This is **not** trivial because each node has multiple connections inside engine and
/// in editor's data model, so we have to thoroughly build command using simple commands.
pub fn make_delete_selection_command(
    editor_scene: &EditorScene,
    engine: &GameEngine,
) -> SceneCommand {
    let graph = &engine.scenes[editor_scene.scene].graph;

    // Graph's root is non-deletable.
    let mut selection = if let Selection::Graph(selection) = &editor_scene.selection {
        selection.clone()
    } else {
        Default::default()
    };
    if let Some(root_position) = selection.nodes.iter().position(|&n| n == graph.get_root()) {
        selection.nodes.remove(root_position);
    }

    // Change selection first.
    let mut command_group = CommandGroup::from(vec![SceneCommand::ChangeSelection(
        ChangeSelectionCommand::new(Default::default(), Selection::Graph(selection.clone())),
    )]);

    // Find sub-graphs to delete - we need to do this because we can end up in situation like this:
    // A_
    //   B_      <-
    //   | C       | these are selected
    //   | D_    <-
    //   |   E
    //   F
    // In this case we must deleted only node B, there is no need to delete node D separately because
    // by engine's design when we delete a node, we also delete all its children. So we have to keep
    // this behaviour in editor too.

    let root_nodes = selection.root_nodes(graph);

    // Delete all associated physics entities in the whole hierarchy starting from root nodes
    // found above.
    let mut stack = root_nodes.clone();
    while let Some(node) = stack.pop() {
        if let Some(&body) = editor_scene.physics.binder.value_of(&node) {
            for &collider in editor_scene.physics.bodies[body].colliders.iter() {
                command_group.push(SceneCommand::DeleteCollider(DeleteColliderCommand::new(
                    collider.into(),
                )))
            }

            command_group.push(SceneCommand::DeleteBody(DeleteBodyCommand::new(body)));

            // Remove any associated joints.
            let joint = editor_scene.physics.find_joint(body);
            if joint.is_some() {
                command_group.push(SceneCommand::DeleteJoint(DeleteJointCommand::new(joint)));
            }

            // Also check if this node is attached to a joint as
            // "connected body".
            for (handle, joint) in editor_scene.physics.joints.pair_iter() {
                if joint.body2 == ErasedHandle::from(body) {
                    command_group.push(SceneCommand::SetJointConnectedBody(
                        SetJointConnectedBodyCommand::new(handle, ErasedHandle::none()),
                    ));
                }
            }
        }
        stack.extend_from_slice(graph[node].children());
    }

    for root_node in root_nodes {
        command_group.push(SceneCommand::DeleteSubGraph(DeleteSubGraphCommand::new(
            root_node,
        )));
    }

    SceneCommand::CommandGroup(command_group)
}
