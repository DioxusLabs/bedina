use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::{Core3d, Core3dSystems};
use bevy::prelude::*;
use bevy::{
    input::{
        keyboard::{Key as BevyKey, KeyCode as BevyKeyCode, KeyboardInput},
        mouse::{MouseButton, MouseButtonInput},
        ButtonInput, ButtonState, InputSystems,
    },
    render::{
        render_asset::RenderAssets,
        render_resource::{Extent3d, TextureDimension, TextureFormat},
        renderer::{RenderDevice, RenderQueue},
        texture::GpuImage,
        Extract, RenderApp,
    },
    window::{CursorMoved, WindowResized},
};

use anyrender_vello::VelloScenePainter;
use blitz_dom::{Document as _, DocumentConfig};
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton,
    MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::net::{NetHandler, NetProvider, NetWaker};
use blitz_traits::shell::{ColorScheme, Viewport};
use blitz_traits::{NodeId, SmolStr};
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use data_url::DataUrl;
use dioxus::prelude::*;
use dioxus_devtools::DevserverMsg;
use dioxus_native_dom::DioxusDocument;
use vello::{
    peniko::color::AlphaColor, RenderParams, Renderer as VelloRenderer, RendererOptions, Scene,
};

// Constant scale factor and color scheme for example purposes
const SCALE_FACTOR: f32 = 1.0;
const COLOR_SCHEME: ColorScheme = ColorScheme::Light;
const CATCH_EVENTS_CLASS: &str = "catch-events";

pub struct DioxusInBevyPlugin<UIProps> {
    pub ui: fn(props: UIProps) -> Element,
    pub props: UIProps,
}

#[derive(Resource)]
struct AnimationTime(Instant);

impl<UIProps: std::marker::Send + std::marker::Sync + std::clone::Clone + 'static> Plugin
    for DioxusInBevyPlugin<UIProps>
{
    fn build(&self, app: &mut App) {
        let epoch = AnimationTime(Instant::now());
        let (s, r) = crossbeam_channel::unbounded();

        // Create the dioxus virtual dom and the dioxus-native document
        // The viewport will be set in setup_ui after we get the window size
        let vdom = VirtualDom::new_with_props(self.ui, self.props.clone());
        // FIXME add a NetProvider
        let mut dioxus_doc = DioxusDocument::new(vdom, DocumentConfig::default());

        // Setup NetProvider
        let net_provider = BevyNetProvider::shared(Some(Arc::new({
            let s = s.clone();
            move |doc_id| {
                s.send(DioxusMessage::ResourceLoad(doc_id)).unwrap();
            }
        })));
        dioxus_doc.inner_mut().set_net_provider(net_provider);

        // Setup DocumentProxy to process CreateHeadElement messages
        let proxy = Rc::new(DioxusDocumentProxy::new(s.clone()));
        dioxus_doc.vdom.in_scope(ScopeId::ROOT, move || {
            provide_context(proxy as Rc<dyn dioxus::document::Document>);
        });

        dioxus_doc.initial_build();
        dioxus_doc.inner_mut().resolve(0.0);

        // Dummy waker
        struct NullWake;
        impl std::task::Wake for NullWake {
            fn wake(self: std::sync::Arc<Self>) {}
        }
        let waker = std::task::Waker::from(std::sync::Arc::new(NullWake));

        // Setup devtools listener for hot-reloading
        dioxus_devtools::connect(move |msg| s.send(DioxusMessage::Devserver(msg)).unwrap());
        app.insert_resource(DioxusMessages(r));

        app.insert_non_send(dioxus_doc);
        app.insert_non_send(waker);
        app.insert_resource(epoch);

        app.add_systems(Startup, setup_ui);
        app.add_systems(
            PreUpdate,
            (
                handle_window_resize,
                handle_mouse_events.after(InputSystems),
                handle_keyboard_events.after(InputSystems),
            )
                .chain(),
        );
        app.add_systems(Update, update_ui);
    }

    fn finish(&self, app: &mut App) {
        // Add the UI rendrer
        let render_app = app.sub_app(RenderApp);
        let render_device = render_app.world().resource::<RenderDevice>();
        let device = render_device.wgpu_device();
        let vello_renderer = VelloRenderer::new(device, RendererOptions::default()).unwrap();
        app.insert_non_send(vello_renderer);

        // Setup communication between main world and render world, to send
        // and receive the texture
        let (s, r) = crossbeam_channel::unbounded();
        app.insert_resource(MainWorldReceiver(r));
        let render_app = app.sub_app_mut(RenderApp);
        render_app.add_systems(bevy::render::ExtractSchedule, extract_texture_image);
        render_app.insert_resource(RenderWorldSender(s));

        // // Add a render graph node to get the GPU texture
        // let mut graph = render_app.world_mut().resource_mut::<RenderGraph>();
        // graph.add_node(TextureGetterNode, TextureGetterNodeDriver);
        // graph.add_node_edge(bevy::render::graph::CameraDriverLabel, TextureGetterNode);

        render_app.add_systems(
            Core3d,
            texture_getter_render_system.after(Core3dSystems::MainPass),
        );
    }
}

struct RenderTexture {
    pub texture_view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

#[derive(Resource, Deref)]
struct MainWorldReceiver(Receiver<RenderTexture>);

#[derive(Resource, Deref)]
struct RenderWorldSender(Sender<RenderTexture>);

fn create_ui_texture(width: u32, height: u32) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0u8; 4],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage = wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::STORAGE_BINDING
        | wgpu::TextureUsages::TEXTURE_BINDING;
    image
}

// #[derive(Debug, PartialEq, Eq, Clone, Hash, RenderLabel)]
// struct TextureGetterNode;

// #[derive(Default)]
// struct TextureGetterNodeDriver;

// impl render_graph::Node for TextureGetterNodeDriver {
//     fn update(&mut self, world: &mut World) {}
//     fn run(
//         &self,
//         _graph: &mut RenderGraphContext,
//         _render_context: &mut RenderContext,
//         _world: &World,
//     ) -> bevy::prelude::Result<(), NodeRunError> {
//         Ok(())
//     }
// }

fn texture_getter_render_system(
    sender: Option<Res<RenderWorldSender>>,
    render_assets: Option<Res<RenderAssets<GpuImage>>>,
    mut extracted_image: Option<ResMut<ExtractedTextureImage>>,
) {
    // Get the GPU texture from the texture image, and send it to the main world
    if let Some(sender) = sender {
        if let Some(image) = extracted_image.as_mut() {
            if let Some(image_id) = image.0.as_ref() {
                if let Some(gpu_image) = render_assets
                    .as_ref()
                    .and_then(|assets| assets.get(image_id))
                {
                    let size = gpu_image.size_2d();
                    let _ = sender.send(RenderTexture {
                        texture_view: (*gpu_image.texture_view).clone(),
                        width: size.x,
                        height: size.y,
                    });
                    // Reset the image, so it is not sent again, unless it changes
                    image.0 = None;
                }
            }
        }
    }

    // // Get the GPU texture from the texture image, and send it to the main world
    // if let Some(sender) = world.get_resource::<RenderWorldSender>() {
    //     if let Some(image) = world
    //         .get_resource::<ExtractedTextureImage>()
    //         .and_then(|e| e.0.as_ref())
    //     {
    //         if let Some(gpu_images) = world
    //             .get_resource::<RenderAssets<GpuImage>>()
    //             .and_then(|a| a.get(image))
    //         {
    //             let _ = sender.send(RenderTexture {
    //                 texture_view: (*gpu_images.texture_view).clone(),
    //                 width: gpu_images.size.width,
    //                 height: gpu_images.size.height,
    //             });
    //             if let Some(mut extracted_image) =
    //                 world.get_resource_mut::<ExtractedTextureImage>()
    //             {
    //                 // Reset the image, so it is not sent again, unless it changes
    //                 extracted_image.0 = None;
    //             }
    //         }
    //     }
}

#[derive(Resource)]
pub struct TextureImage(Handle<Image>);

#[derive(Resource)]
pub struct ExtractedTextureImage(Option<Handle<Image>>);

fn extract_texture_image(
    mut commands: Commands,
    texture_image: Extract<Option<Res<TextureImage>>>,
    mut last_texture_image: Local<Option<Handle<Image>>>,
) {
    if let Some(texture_image) = texture_image.as_ref() {
        if let Some(last_texture_image) = &*last_texture_image {
            if last_texture_image == &texture_image.0 {
                return;
            }
        }
        commands.insert_resource(ExtractedTextureImage(Some(texture_image.0.clone())));
        *last_texture_image = Some(texture_image.0.clone());
    }
}

struct HeadElement {
    name: String,
    attributes: Vec<(String, String)>,
    contents: Option<String>,
}

enum DioxusMessage {
    Devserver(DevserverMsg),
    CreateHeadElement(HeadElement),
    #[allow(dead_code)]
    ResourceLoad(usize),
}

#[derive(Resource, Deref)]
struct DioxusMessages(Receiver<DioxusMessage>);

#[derive(Component)]
pub struct DioxusUiQuad;

fn setup_ui(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut dioxus_doc: NonSendMut<DioxusDocument>,
    mut animation_epoch: ResMut<AnimationTime>,
    windows: Query<&Window>,
) {
    let window = windows
        .iter()
        .next()
        .expect("Should have at least one window");
    let width = window.physical_width();
    let height = window.physical_height();

    debug!("Initial window size: {}x{}", width, height);

    // Set the initial viewport
    animation_epoch.0 = Instant::now();
    dioxus_doc
        .inner_mut()
        .set_viewport(Viewport::new(width, height, SCALE_FACTOR, COLOR_SCHEME));
    dioxus_doc.inner_mut().resolve(0.0);

    // Create Bevy Image from the texture data
    let image = create_ui_texture(width, height);
    let handle = images.add(image);

    // Create a quad to display the texture
    commands.spawn((
        Mesh2d(meshes.add(Rectangle::new(1.0, 1.0))),
        MeshMaterial2d(materials.add(ColorMaterial {
            texture: Some(handle.clone()),
            ..default()
        })),
        Transform::from_scale(Vec3::new(width as f32, height as f32, 0.0)),
        DioxusUiQuad,
    ));
    commands.spawn((
        Camera2d,
        Camera {
            order: isize::MAX,
            ..default()
        },
    ));

    commands.insert_resource(TextureImage(handle));
}

#[allow(clippy::too_many_arguments)]
fn update_ui(
    mut dioxus_doc: NonSendMut<DioxusDocument>,
    dioxus_messages: Res<DioxusMessages>,
    waker: NonSendMut<std::task::Waker>,
    vello_renderer: Option<NonSendMut<VelloRenderer>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    receiver: Res<MainWorldReceiver>,
    animation_epoch: Res<AnimationTime>,
    mut cached_texture: Local<Option<RenderTexture>>,
) {
    while let Ok(msg) = dioxus_messages.0.try_recv() {
        match msg {
            DioxusMessage::Devserver(devserver_msg) => match devserver_msg {
                dioxus_devtools::DevserverMsg::HotReload(hotreload_message) => {
                    // Apply changes to vdom
                    dioxus_devtools::apply_changes(&dioxus_doc.vdom, &hotreload_message);

                    // Reload changed assets
                    for asset_path in &hotreload_message.assets {
                        if let Some(url) = asset_path.to_str() {
                            dioxus_doc.inner_mut().reload_resource_by_href(url);
                        }
                    }
                }
                dioxus_devtools::DevserverMsg::FullReloadStart => {}
                _ => {}
            },
            DioxusMessage::CreateHeadElement(el) => {
                dioxus_doc.create_head_element(&el.name, &el.attributes, &el.contents);
                dioxus_doc.poll(Some(std::task::Context::from_waker(&waker)));
            }
            DioxusMessage::ResourceLoad(_) => {
                // Do nothing
            }
        };
    }

    while let Ok(texture) = receiver.try_recv() {
        *cached_texture = Some(texture);
    }

    if let (Some(texture), Some(mut vello_renderer)) = ((*cached_texture).as_ref(), vello_renderer)
    {
        // Poll the vdom
        dioxus_doc.poll(Some(std::task::Context::from_waker(&waker)));

        // Refresh the document
        let animation_time = animation_epoch.0.elapsed().as_secs_f64();
        dioxus_doc.inner_mut().resolve(animation_time);

        // Create a `vello::Scene` to paint into
        let mut scene = Scene::new();

        // Paint the document
        paint_scene(
            &mut VelloScenePainter::new(&mut scene),
            &mut dioxus_doc.inner_mut(),
            SCALE_FACTOR as f64,
            texture.width,
            texture.height,
            0,
            0,
        );

        // Render the `vello::Scene` to the Texture using the `VelloRenderer`
        vello_renderer
            .render_to_texture(
                render_device.wgpu_device(),
                render_queue.into_inner(),
                &scene,
                &texture.texture_view,
                &RenderParams {
                    base_color: AlphaColor::TRANSPARENT,
                    width: texture.width,
                    height: texture.height,
                    antialiasing_method: vello::AaConfig::Msaa16,
                },
            )
            .expect("failed to render to texture");
    }
}

fn handle_window_resize(
    mut dioxus_doc: NonSendMut<DioxusDocument>,
    mut resize_events: MessageReader<WindowResized>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    texture_image: Option<Res<TextureImage>>,
    mut query: Query<(&mut Transform, &mut MeshMaterial2d<ColorMaterial>), With<DioxusUiQuad>>,
) {
    for resize_event in resize_events.read() {
        let width = resize_event.width as u32;
        let height = resize_event.height as u32;

        debug!("Window resized to: {}x{}", width, height);

        // Update the dioxus viewport
        dioxus_doc.inner_mut().set_viewport(Viewport::new(
            width,
            height,
            SCALE_FACTOR,
            COLOR_SCHEME,
        ));
        // dioxus_doc.resolve();

        // Create a new texture with the new size
        let new_image = create_ui_texture(width, height);
        let new_handle = images.add(new_image);

        // Update the quad mesh to match the new size
        if let Ok((mut trans, mut mat)) = query.single_mut() {
            *trans = Transform::from_scale(Vec3::new(width as f32, height as f32, 0.0));
            materials.get_mut(&mut mat.0).unwrap().texture = Some(new_handle.clone());
        }

        // Remove the old texture
        if let Some(texture_image) = texture_image.as_ref() {
            images.remove(&texture_image.0);
        }

        // Insert the new texture resource
        commands.insert_resource(TextureImage(new_handle));
    }
}

#[derive(Resource, Default)]
pub struct MouseState {
    pub x: f32,
    pub y: f32,
    pub buttons: MouseEventButtons,
    pub mods: Modifiers,
}

fn does_catch_events(dioxus_doc: &DioxusDocument, node_id: NodeId) -> bool {
    if let Some(node) = dioxus_doc.inner().get_node(node_id) {
        let class = node.attr(blitz_dom::local_name!("class")).unwrap_or("");
        if class
            .split_whitespace()
            .any(|word| word == CATCH_EVENTS_CLASS)
        {
            true
        } else if let Some(parent) = node.parent {
            does_catch_events(dioxus_doc, parent)
        } else {
            false
        }
    } else {
        false
    }
}

fn handle_mouse_events(
    mut dioxus_doc: NonSendMut<DioxusDocument>,
    mut cursor_moved: MessageReader<CursorMoved>,
    mut mouse_button_input_events: ResMut<Messages<MouseButtonInput>>,
    mut mouse_buttons: ResMut<ButtonInput<MouseButton>>,
    mut last_mouse_state: Local<MouseState>,
) {
    if cursor_moved.is_empty() && mouse_button_input_events.is_empty() {
        return;
    }

    let mouse_state = &mut last_mouse_state;

    for cursor_event in cursor_moved.read() {
        mouse_state.x = cursor_event.position.x;
        mouse_state.y = cursor_event.position.y;
        dioxus_doc.handle_ui_event(UiEvent::PointerMove(BlitzPointerEvent {
            id: BlitzPointerId::MOUSE,
            is_primary: true,
            coords: PointerCoords {
                client_x: mouse_state.x,
                client_y: mouse_state.y,
                page_x: mouse_state.x,
                page_y: mouse_state.y,
                screen_x: mouse_state.x,
                screen_y: mouse_state.y,
            },
            button: Default::default(),
            buttons: mouse_state.buttons,
            mods: mouse_state.mods,
            details: PointerDetails::default(),
            element: Default::default(),
            active_pointers: Default::default(),
        }));
    }

    for event in mouse_button_input_events
        .get_cursor()
        .read(&mouse_button_input_events)
    {
        let button_blitz = match event.button {
            MouseButton::Left => MouseEventButton::Main,
            MouseButton::Right => MouseEventButton::Secondary,
            MouseButton::Middle => MouseEventButton::Auxiliary,
            MouseButton::Back => MouseEventButton::Fourth,
            MouseButton::Forward => MouseEventButton::Fifth,
            _ => continue,
        };
        let buttons_blitz = MouseEventButtons::from(button_blitz);
        match event.state {
            ButtonState::Pressed => {
                mouse_state.buttons |= buttons_blitz;
                dioxus_doc.handle_ui_event(UiEvent::PointerDown(BlitzPointerEvent {
                    id: BlitzPointerId::MOUSE,
                    is_primary: true,
                    coords: PointerCoords {
                        client_x: mouse_state.x,
                        client_y: mouse_state.y,
                        page_x: mouse_state.x,
                        page_y: mouse_state.y,
                        screen_x: mouse_state.x,
                        screen_y: mouse_state.y,
                    },
                    button: button_blitz,
                    buttons: mouse_state.buttons,
                    mods: mouse_state.mods,
                    details: PointerDetails::default(),
                    element: Default::default(),
                    active_pointers: Default::default(),
                }));
            }
            ButtonState::Released => {
                mouse_state.buttons &= !buttons_blitz;
                dioxus_doc.handle_ui_event(UiEvent::PointerUp(BlitzPointerEvent {
                    id: BlitzPointerId::MOUSE,
                    is_primary: true,
                    coords: PointerCoords {
                        client_x: mouse_state.x,
                        client_y: mouse_state.y,
                        page_x: mouse_state.x,
                        page_y: mouse_state.y,
                        screen_x: mouse_state.x,
                        screen_y: mouse_state.y,
                    },
                    button: button_blitz,
                    buttons: mouse_state.buttons,
                    mods: mouse_state.mods,
                    details: PointerDetails::default(),
                    element: Default::default(),
                    active_pointers: Default::default(),
                }));
            }
        }
    }

    let inner = dioxus_doc.inner();
    let should_catch_events = inner
        .hit(mouse_state.x, mouse_state.y)
        .map(|hit| does_catch_events(&dioxus_doc, hit.node_id))
        .unwrap_or(false);
    if should_catch_events {
        mouse_button_input_events.clear();
        mouse_buttons.reset_all();
    }

    // dioxus_doc.resolve();
}

fn handle_keyboard_events(
    mut dioxus_doc: NonSendMut<DioxusDocument>,
    mut keyboard_input_events: ResMut<Messages<KeyboardInput>>,
    mut keys: ResMut<ButtonInput<BevyKeyCode>>,
    mut last_mouse_state: Local<MouseState>,
) {
    if keyboard_input_events.is_empty() {
        return;
    }

    for event in keyboard_input_events
        .get_cursor()
        .read(&keyboard_input_events)
    {
        let modifier = match event.logical_key {
            BevyKey::Alt => Some(Modifiers::ALT),
            BevyKey::AltGraph => Some(Modifiers::ALT_GRAPH),
            BevyKey::CapsLock => Some(Modifiers::CAPS_LOCK),
            BevyKey::Control => Some(Modifiers::CONTROL),
            BevyKey::Fn => Some(Modifiers::FN),
            BevyKey::FnLock => Some(Modifiers::FN_LOCK),
            BevyKey::NumLock => Some(Modifiers::NUM_LOCK),
            BevyKey::ScrollLock => Some(Modifiers::SCROLL_LOCK),
            BevyKey::Shift => Some(Modifiers::SHIFT),
            BevyKey::Symbol => Some(Modifiers::SYMBOL),
            BevyKey::SymbolLock => Some(Modifiers::SYMBOL_LOCK),
            BevyKey::Meta => Some(Modifiers::META),
            BevyKey::Hyper => Some(Modifiers::HYPER),
            BevyKey::Super => Some(Modifiers::SUPER),
            _ => None,
        };
        if let Some(modifier) = modifier {
            match event.state {
                ButtonState::Pressed => last_mouse_state.mods.insert(modifier),
                ButtonState::Released => last_mouse_state.mods.remove(modifier),
            };
        }
        let key_state = match event.state {
            ButtonState::Pressed => KeyState::Pressed,
            ButtonState::Released => KeyState::Released,
        };
        let blitz_key_event = BlitzKeyEvent {
            key: bevy_key_to_blitz_key(&event.logical_key),
            code: bevy_key_code_to_blitz_code(&event.key_code),
            modifiers: last_mouse_state.mods,
            location: Location::Standard,
            is_auto_repeating: event.repeat,
            is_composing: false,
            state: key_state,
            // Mismatched versions of SmolStr
            text: event.text.as_ref().map(SmolStr::new),
        };

        match key_state {
            KeyState::Pressed => {
                dioxus_doc.handle_ui_event(UiEvent::KeyDown(blitz_key_event));
            }
            KeyState::Released => {
                dioxus_doc.handle_ui_event(UiEvent::KeyUp(blitz_key_event));
            }
        }
    }

    let should_catch_events = dioxus_doc
        .inner()
        .hit(last_mouse_state.x, last_mouse_state.y)
        .map(|hit| does_catch_events(&dioxus_doc, hit.node_id))
        .unwrap_or(false);
    if should_catch_events {
        keyboard_input_events.clear();
        keys.reset_all();
    }

    // dioxus_doc.resolve();
}

pub struct DioxusDocumentProxy {
    sender: Sender<DioxusMessage>,
}

impl DioxusDocumentProxy {
    fn new(sender: Sender<DioxusMessage>) -> Self {
        Self { sender }
    }
}

impl dioxus::document::Document for DioxusDocumentProxy {
    fn eval(&self, _js: String) -> dioxus::document::Eval {
        dioxus::document::NoOpDocument.eval(_js)
    }

    fn create_head_element(
        &self,
        name: &str,
        attributes: &[(&str, String)],
        contents: Option<String>,
    ) {
        self.sender
            .send(DioxusMessage::CreateHeadElement(HeadElement {
                name: name.to_string(),
                attributes: attributes
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
                contents,
            }))
            .unwrap();
    }

    fn set_title(&self, title: String) {
        self.create_head_element("title", &[], Some(title));
    }

    fn create_meta(&self, props: dioxus::document::MetaProps) {
        let attributes = props.attributes();
        self.create_head_element("meta", &attributes, None);
    }

    fn create_script(&self, props: dioxus::document::ScriptProps) {
        let attributes = props.attributes();
        self.create_head_element("script", &attributes, props.script_contents().ok());
    }

    fn create_style(&self, props: dioxus::document::StyleProps) {
        let attributes = props.attributes();
        self.create_head_element("style", &attributes, props.style_contents().ok());
    }

    fn create_link(&self, props: dioxus::document::LinkProps) {
        let attributes = props.attributes();
        self.create_head_element("link", &attributes, None);
    }

    fn create_head_component(&self) -> bool {
        true
    }
}

pub struct BevyNetProvider {
    waker: Option<Arc<dyn NetWaker>>,
}
impl BevyNetProvider {
    fn shared(waker: Option<Arc<dyn NetWaker>>) -> Arc<dyn NetProvider> {
        Arc::new(Self::new(waker)) as _
    }

    fn new(waker: Option<Arc<dyn NetWaker>>) -> Self {
        Self { waker }
    }
}

impl NetProvider for BevyNetProvider {
    fn fetch(
        &self,
        doc_id: usize,
        request: blitz_traits::net::Request,
        handler: Box<dyn NetHandler>,
    ) {
        match request.url.scheme() {
            // Load Dioxus assets
            "dioxus" => {
                if let Ok(res) = dioxus_asset_resolver::native::serve_asset(request.url.path()) {
                    handler.bytes(request.url.to_string(), res.into_body().into());
                }
            }
            // Decode data URIs
            "data" => {
                let Ok(data_url) = DataUrl::process(request.url.as_str()) else {
                    return;
                };
                let Ok(decoded) = data_url.decode_to_vec() else {
                    return;
                };
                let bytes = Bytes::from(decoded.0);
                handler.bytes(request.url.to_string(), bytes);
            }
            // TODO: support http requests
            _ => {}
        }

        if let Some(waker) = &self.waker {
            waker.wake(doc_id);
        }
    }
}

fn bevy_key_to_blitz_key(key: &BevyKey) -> Key {
    match key {
        BevyKey::Character(c) => Key::Character(c.to_string()),
        BevyKey::Unidentified(_) => Key::Named(NamedKey::Unidentified),
        BevyKey::Dead(_) => Key::Named(NamedKey::Dead),
        BevyKey::Alt => Key::Named(NamedKey::Alt),
        BevyKey::AltGraph => Key::Named(NamedKey::AltGraph),
        BevyKey::CapsLock => Key::Named(NamedKey::CapsLock),
        BevyKey::Control => Key::Named(NamedKey::Control),
        BevyKey::Fn => Key::Named(NamedKey::Fn),
        BevyKey::FnLock => Key::Named(NamedKey::FnLock),
        BevyKey::NumLock => Key::Named(NamedKey::NumLock),
        BevyKey::ScrollLock => Key::Named(NamedKey::ScrollLock),
        BevyKey::Shift => Key::Named(NamedKey::Shift),
        BevyKey::Symbol => Key::Named(NamedKey::Symbol),
        BevyKey::SymbolLock => Key::Named(NamedKey::SymbolLock),
        BevyKey::Meta => Key::Named(NamedKey::Meta),
        BevyKey::Hyper => Key::Named(NamedKey::Hyper),
        BevyKey::Super => Key::Named(NamedKey::Super),
        BevyKey::Enter => Key::Named(NamedKey::Enter),
        BevyKey::Tab => Key::Named(NamedKey::Tab),
        BevyKey::Space => Key::Character(" ".to_string()),
        BevyKey::ArrowDown => Key::Named(NamedKey::ArrowDown),
        BevyKey::ArrowLeft => Key::Named(NamedKey::ArrowLeft),
        BevyKey::ArrowRight => Key::Named(NamedKey::ArrowRight),
        BevyKey::ArrowUp => Key::Named(NamedKey::ArrowUp),
        BevyKey::End => Key::Named(NamedKey::End),
        BevyKey::Home => Key::Named(NamedKey::Home),
        BevyKey::PageDown => Key::Named(NamedKey::PageDown),
        BevyKey::PageUp => Key::Named(NamedKey::PageUp),
        BevyKey::Backspace => Key::Named(NamedKey::Backspace),
        BevyKey::Clear => Key::Named(NamedKey::Clear),
        BevyKey::Copy => Key::Named(NamedKey::Copy),
        BevyKey::CrSel => Key::Named(NamedKey::CrSel),
        BevyKey::Cut => Key::Named(NamedKey::Cut),
        BevyKey::Delete => Key::Named(NamedKey::Delete),
        BevyKey::EraseEof => Key::Named(NamedKey::EraseEof),
        BevyKey::ExSel => Key::Named(NamedKey::ExSel),
        BevyKey::Insert => Key::Named(NamedKey::Insert),
        BevyKey::Paste => Key::Named(NamedKey::Paste),
        BevyKey::Redo => Key::Named(NamedKey::Redo),
        BevyKey::Undo => Key::Named(NamedKey::Undo),
        BevyKey::Accept => Key::Named(NamedKey::Accept),
        BevyKey::Again => Key::Named(NamedKey::Again),
        BevyKey::Attn => Key::Named(NamedKey::Attn),
        BevyKey::Cancel => Key::Named(NamedKey::Cancel),
        BevyKey::ContextMenu => Key::Named(NamedKey::ContextMenu),
        BevyKey::Escape => Key::Named(NamedKey::Escape),
        BevyKey::Execute => Key::Named(NamedKey::Execute),
        BevyKey::Find => Key::Named(NamedKey::Find),
        BevyKey::Help => Key::Named(NamedKey::Help),
        BevyKey::Pause => Key::Named(NamedKey::Pause),
        BevyKey::Play => Key::Named(NamedKey::Play),
        BevyKey::Props => Key::Named(NamedKey::Props),
        BevyKey::Select => Key::Named(NamedKey::Select),
        BevyKey::ZoomIn => Key::Named(NamedKey::ZoomIn),
        BevyKey::ZoomOut => Key::Named(NamedKey::ZoomOut),
        BevyKey::BrightnessDown => Key::Named(NamedKey::BrightnessDown),
        BevyKey::BrightnessUp => Key::Named(NamedKey::BrightnessUp),
        BevyKey::Eject => Key::Named(NamedKey::Eject),
        BevyKey::LogOff => Key::Named(NamedKey::LogOff),
        BevyKey::Power => Key::Named(NamedKey::Power),
        BevyKey::PowerOff => Key::Named(NamedKey::PowerOff),
        BevyKey::PrintScreen => Key::Named(NamedKey::PrintScreen),
        BevyKey::Hibernate => Key::Named(NamedKey::Hibernate),
        BevyKey::Standby => Key::Named(NamedKey::Standby),
        BevyKey::WakeUp => Key::Named(NamedKey::WakeUp),
        BevyKey::AllCandidates => Key::Named(NamedKey::AllCandidates),
        BevyKey::Alphanumeric => Key::Named(NamedKey::Alphanumeric),
        BevyKey::CodeInput => Key::Named(NamedKey::CodeInput),
        BevyKey::Compose => Key::Named(NamedKey::Compose),
        BevyKey::Convert => Key::Named(NamedKey::Convert),
        BevyKey::FinalMode => Key::Named(NamedKey::FinalMode),
        BevyKey::GroupFirst => Key::Named(NamedKey::GroupFirst),
        BevyKey::GroupLast => Key::Named(NamedKey::GroupLast),
        BevyKey::GroupNext => Key::Named(NamedKey::GroupNext),
        BevyKey::GroupPrevious => Key::Named(NamedKey::GroupPrevious),
        BevyKey::ModeChange => Key::Named(NamedKey::ModeChange),
        BevyKey::NextCandidate => Key::Named(NamedKey::NextCandidate),
        BevyKey::NonConvert => Key::Named(NamedKey::NonConvert),
        BevyKey::PreviousCandidate => Key::Named(NamedKey::PreviousCandidate),
        BevyKey::Process => Key::Named(NamedKey::Process),
        BevyKey::SingleCandidate => Key::Named(NamedKey::SingleCandidate),
        BevyKey::HangulMode => Key::Named(NamedKey::HangulMode),
        BevyKey::HanjaMode => Key::Named(NamedKey::HanjaMode),
        BevyKey::JunjaMode => Key::Named(NamedKey::JunjaMode),
        BevyKey::Eisu => Key::Named(NamedKey::Eisu),
        BevyKey::Hankaku => Key::Named(NamedKey::Hankaku),
        BevyKey::Hiragana => Key::Named(NamedKey::Hiragana),
        BevyKey::HiraganaKatakana => Key::Named(NamedKey::HiraganaKatakana),
        BevyKey::KanaMode => Key::Named(NamedKey::KanaMode),
        BevyKey::KanjiMode => Key::Named(NamedKey::KanjiMode),
        BevyKey::Katakana => Key::Named(NamedKey::Katakana),
        BevyKey::Romaji => Key::Named(NamedKey::Romaji),
        BevyKey::Zenkaku => Key::Named(NamedKey::Zenkaku),
        BevyKey::ZenkakuHankaku => Key::Named(NamedKey::ZenkakuHankaku),
        BevyKey::Soft1 => Key::Named(NamedKey::Soft1),
        BevyKey::Soft2 => Key::Named(NamedKey::Soft2),
        BevyKey::Soft3 => Key::Named(NamedKey::Soft3),
        BevyKey::Soft4 => Key::Named(NamedKey::Soft4),
        BevyKey::ChannelDown => Key::Named(NamedKey::ChannelDown),
        BevyKey::ChannelUp => Key::Named(NamedKey::ChannelUp),
        BevyKey::Close => Key::Named(NamedKey::Close),
        BevyKey::MailForward => Key::Named(NamedKey::MailForward),
        BevyKey::MailReply => Key::Named(NamedKey::MailReply),
        BevyKey::MailSend => Key::Named(NamedKey::MailSend),
        BevyKey::MediaClose => Key::Named(NamedKey::MediaClose),
        BevyKey::MediaFastForward => Key::Named(NamedKey::MediaFastForward),
        BevyKey::MediaPause => Key::Named(NamedKey::MediaPause),
        BevyKey::MediaPlay => Key::Named(NamedKey::MediaPlay),
        BevyKey::MediaPlayPause => Key::Named(NamedKey::MediaPlayPause),
        BevyKey::MediaRecord => Key::Named(NamedKey::MediaRecord),
        BevyKey::MediaRewind => Key::Named(NamedKey::MediaRewind),
        BevyKey::MediaStop => Key::Named(NamedKey::MediaStop),
        BevyKey::MediaTrackNext => Key::Named(NamedKey::MediaTrackNext),
        BevyKey::MediaTrackPrevious => Key::Named(NamedKey::MediaTrackPrevious),
        BevyKey::New => Key::Named(NamedKey::New),
        BevyKey::Open => Key::Named(NamedKey::Open),
        BevyKey::Print => Key::Named(NamedKey::Print),
        BevyKey::Save => Key::Named(NamedKey::Save),
        BevyKey::SpellCheck => Key::Named(NamedKey::SpellCheck),
        BevyKey::Key11 => Key::Named(NamedKey::Key11),
        BevyKey::Key12 => Key::Named(NamedKey::Key12),
        BevyKey::AudioBalanceLeft => Key::Named(NamedKey::AudioBalanceLeft),
        BevyKey::AudioBalanceRight => Key::Named(NamedKey::AudioBalanceRight),
        BevyKey::AudioBassBoostDown => Key::Named(NamedKey::AudioBassBoostDown),
        BevyKey::AudioBassBoostToggle => Key::Named(NamedKey::AudioBassBoostToggle),
        BevyKey::AudioBassBoostUp => Key::Named(NamedKey::AudioBassBoostUp),
        BevyKey::AudioFaderFront => Key::Named(NamedKey::AudioFaderFront),
        BevyKey::AudioFaderRear => Key::Named(NamedKey::AudioFaderRear),
        BevyKey::AudioSurroundModeNext => Key::Named(NamedKey::AudioSurroundModeNext),
        BevyKey::AudioTrebleDown => Key::Named(NamedKey::AudioTrebleDown),
        BevyKey::AudioTrebleUp => Key::Named(NamedKey::AudioTrebleUp),
        BevyKey::AudioVolumeDown => Key::Named(NamedKey::AudioVolumeDown),
        BevyKey::AudioVolumeUp => Key::Named(NamedKey::AudioVolumeUp),
        BevyKey::AudioVolumeMute => Key::Named(NamedKey::AudioVolumeMute),
        BevyKey::MicrophoneToggle => Key::Named(NamedKey::MicrophoneToggle),
        BevyKey::MicrophoneVolumeDown => Key::Named(NamedKey::MicrophoneVolumeDown),
        BevyKey::MicrophoneVolumeUp => Key::Named(NamedKey::MicrophoneVolumeUp),
        BevyKey::MicrophoneVolumeMute => Key::Named(NamedKey::MicrophoneVolumeMute),
        BevyKey::SpeechCorrectionList => Key::Named(NamedKey::SpeechCorrectionList),
        BevyKey::SpeechInputToggle => Key::Named(NamedKey::SpeechInputToggle),
        BevyKey::LaunchApplication1 => Key::Named(NamedKey::LaunchApplication1),
        BevyKey::LaunchApplication2 => Key::Named(NamedKey::LaunchApplication2),
        BevyKey::LaunchCalendar => Key::Named(NamedKey::LaunchCalendar),
        BevyKey::LaunchContacts => Key::Named(NamedKey::LaunchContacts),
        BevyKey::LaunchMail => Key::Named(NamedKey::LaunchMail),
        BevyKey::LaunchMediaPlayer => Key::Named(NamedKey::LaunchMediaPlayer),
        BevyKey::LaunchMusicPlayer => Key::Named(NamedKey::LaunchMusicPlayer),
        BevyKey::LaunchPhone => Key::Named(NamedKey::LaunchPhone),
        BevyKey::LaunchScreenSaver => Key::Named(NamedKey::LaunchScreenSaver),
        BevyKey::LaunchSpreadsheet => Key::Named(NamedKey::LaunchSpreadsheet),
        BevyKey::LaunchWebBrowser => Key::Named(NamedKey::LaunchWebBrowser),
        BevyKey::LaunchWebCam => Key::Named(NamedKey::LaunchWebCam),
        BevyKey::LaunchWordProcessor => Key::Named(NamedKey::LaunchWordProcessor),
        BevyKey::BrowserBack => Key::Named(NamedKey::BrowserBack),
        BevyKey::BrowserFavorites => Key::Named(NamedKey::BrowserFavorites),
        BevyKey::BrowserForward => Key::Named(NamedKey::BrowserForward),
        BevyKey::BrowserHome => Key::Named(NamedKey::BrowserHome),
        BevyKey::BrowserRefresh => Key::Named(NamedKey::BrowserRefresh),
        BevyKey::BrowserSearch => Key::Named(NamedKey::BrowserSearch),
        BevyKey::BrowserStop => Key::Named(NamedKey::BrowserStop),
        BevyKey::AppSwitch => Key::Named(NamedKey::AppSwitch),
        BevyKey::Call => Key::Named(NamedKey::Call),
        BevyKey::Camera => Key::Named(NamedKey::Camera),
        BevyKey::CameraFocus => Key::Named(NamedKey::CameraFocus),
        BevyKey::EndCall => Key::Named(NamedKey::EndCall),
        BevyKey::GoBack => Key::Named(NamedKey::GoBack),
        BevyKey::GoHome => Key::Named(NamedKey::GoHome),
        BevyKey::HeadsetHook => Key::Named(NamedKey::HeadsetHook),
        BevyKey::LastNumberRedial => Key::Named(NamedKey::LastNumberRedial),
        BevyKey::Notification => Key::Named(NamedKey::Notification),
        BevyKey::MannerMode => Key::Named(NamedKey::MannerMode),
        BevyKey::VoiceDial => Key::Named(NamedKey::VoiceDial),
        BevyKey::TV => Key::Named(NamedKey::TV),
        BevyKey::TV3DMode => Key::Named(NamedKey::TV3DMode),
        BevyKey::TVAntennaCable => Key::Named(NamedKey::TVAntennaCable),
        BevyKey::TVAudioDescription => Key::Named(NamedKey::TVAudioDescription),
        BevyKey::TVAudioDescriptionMixDown => Key::Named(NamedKey::TVAudioDescriptionMixDown),
        BevyKey::TVAudioDescriptionMixUp => Key::Named(NamedKey::TVAudioDescriptionMixUp),
        BevyKey::TVContentsMenu => Key::Named(NamedKey::TVContentsMenu),
        BevyKey::TVDataService => Key::Named(NamedKey::TVDataService),
        BevyKey::TVInput => Key::Named(NamedKey::TVInput),
        BevyKey::TVInputComponent1 => Key::Named(NamedKey::TVInputComponent1),
        BevyKey::TVInputComponent2 => Key::Named(NamedKey::TVInputComponent2),
        BevyKey::TVInputComposite1 => Key::Named(NamedKey::TVInputComposite1),
        BevyKey::TVInputComposite2 => Key::Named(NamedKey::TVInputComposite2),
        BevyKey::TVInputHDMI1 => Key::Named(NamedKey::TVInputHDMI1),
        BevyKey::TVInputHDMI2 => Key::Named(NamedKey::TVInputHDMI2),
        BevyKey::TVInputHDMI3 => Key::Named(NamedKey::TVInputHDMI3),
        BevyKey::TVInputHDMI4 => Key::Named(NamedKey::TVInputHDMI4),
        BevyKey::TVInputVGA1 => Key::Named(NamedKey::TVInputVGA1),
        BevyKey::TVMediaContext => Key::Named(NamedKey::TVMediaContext),
        BevyKey::TVNetwork => Key::Named(NamedKey::TVNetwork),
        BevyKey::TVNumberEntry => Key::Named(NamedKey::TVNumberEntry),
        BevyKey::TVPower => Key::Named(NamedKey::TVPower),
        BevyKey::TVRadioService => Key::Named(NamedKey::TVRadioService),
        BevyKey::TVSatellite => Key::Named(NamedKey::TVSatellite),
        BevyKey::TVSatelliteBS => Key::Named(NamedKey::TVSatelliteBS),
        BevyKey::TVSatelliteCS => Key::Named(NamedKey::TVSatelliteCS),
        BevyKey::TVSatelliteToggle => Key::Named(NamedKey::TVSatelliteToggle),
        BevyKey::TVTerrestrialAnalog => Key::Named(NamedKey::TVTerrestrialAnalog),
        BevyKey::TVTerrestrialDigital => Key::Named(NamedKey::TVTerrestrialDigital),
        BevyKey::TVTimer => Key::Named(NamedKey::TVTimer),
        BevyKey::AVRInput => Key::Named(NamedKey::AVRInput),
        BevyKey::AVRPower => Key::Named(NamedKey::AVRPower),
        BevyKey::ColorF0Red => Key::Named(NamedKey::ColorF0Red),
        BevyKey::ColorF1Green => Key::Named(NamedKey::ColorF1Green),
        BevyKey::ColorF2Yellow => Key::Named(NamedKey::ColorF2Yellow),
        BevyKey::ColorF3Blue => Key::Named(NamedKey::ColorF3Blue),
        BevyKey::ColorF4Grey => Key::Named(NamedKey::ColorF4Grey),
        BevyKey::ColorF5Brown => Key::Named(NamedKey::ColorF5Brown),
        BevyKey::ClosedCaptionToggle => Key::Named(NamedKey::ClosedCaptionToggle),
        BevyKey::Dimmer => Key::Named(NamedKey::Dimmer),
        BevyKey::DisplaySwap => Key::Named(NamedKey::DisplaySwap),
        BevyKey::DVR => Key::Named(NamedKey::DVR),
        BevyKey::Exit => Key::Named(NamedKey::Exit),
        BevyKey::FavoriteClear0 => Key::Named(NamedKey::FavoriteClear0),
        BevyKey::FavoriteClear1 => Key::Named(NamedKey::FavoriteClear1),
        BevyKey::FavoriteClear2 => Key::Named(NamedKey::FavoriteClear2),
        BevyKey::FavoriteClear3 => Key::Named(NamedKey::FavoriteClear3),
        BevyKey::FavoriteRecall0 => Key::Named(NamedKey::FavoriteRecall0),
        BevyKey::FavoriteRecall1 => Key::Named(NamedKey::FavoriteRecall1),
        BevyKey::FavoriteRecall2 => Key::Named(NamedKey::FavoriteRecall2),
        BevyKey::FavoriteRecall3 => Key::Named(NamedKey::FavoriteRecall3),
        BevyKey::FavoriteStore0 => Key::Named(NamedKey::FavoriteStore0),
        BevyKey::FavoriteStore1 => Key::Named(NamedKey::FavoriteStore1),
        BevyKey::FavoriteStore2 => Key::Named(NamedKey::FavoriteStore2),
        BevyKey::FavoriteStore3 => Key::Named(NamedKey::FavoriteStore3),
        BevyKey::Guide => Key::Named(NamedKey::Guide),
        BevyKey::GuideNextDay => Key::Named(NamedKey::GuideNextDay),
        BevyKey::GuidePreviousDay => Key::Named(NamedKey::GuidePreviousDay),
        BevyKey::Info => Key::Named(NamedKey::Info),
        BevyKey::InstantReplay => Key::Named(NamedKey::InstantReplay),
        BevyKey::Link => Key::Named(NamedKey::Link),
        BevyKey::ListProgram => Key::Named(NamedKey::ListProgram),
        BevyKey::LiveContent => Key::Named(NamedKey::LiveContent),
        BevyKey::Lock => Key::Named(NamedKey::Lock),
        BevyKey::MediaApps => Key::Named(NamedKey::MediaApps),
        BevyKey::MediaAudioTrack => Key::Named(NamedKey::MediaAudioTrack),
        BevyKey::MediaLast => Key::Named(NamedKey::MediaLast),
        BevyKey::MediaSkipBackward => Key::Named(NamedKey::MediaSkipBackward),
        BevyKey::MediaSkipForward => Key::Named(NamedKey::MediaSkipForward),
        BevyKey::MediaStepBackward => Key::Named(NamedKey::MediaStepBackward),
        BevyKey::MediaStepForward => Key::Named(NamedKey::MediaStepForward),
        BevyKey::MediaTopMenu => Key::Named(NamedKey::MediaTopMenu),
        BevyKey::NavigateIn => Key::Named(NamedKey::NavigateIn),
        BevyKey::NavigateNext => Key::Named(NamedKey::NavigateNext),
        BevyKey::NavigateOut => Key::Named(NamedKey::NavigateOut),
        BevyKey::NavigatePrevious => Key::Named(NamedKey::NavigatePrevious),
        BevyKey::NextFavoriteChannel => Key::Named(NamedKey::NextFavoriteChannel),
        BevyKey::NextUserProfile => Key::Named(NamedKey::NextUserProfile),
        BevyKey::OnDemand => Key::Named(NamedKey::OnDemand),
        BevyKey::Pairing => Key::Named(NamedKey::Pairing),
        BevyKey::PinPDown => Key::Named(NamedKey::PinPDown),
        BevyKey::PinPMove => Key::Named(NamedKey::PinPMove),
        BevyKey::PinPToggle => Key::Named(NamedKey::PinPToggle),
        BevyKey::PinPUp => Key::Named(NamedKey::PinPUp),
        BevyKey::PlaySpeedDown => Key::Named(NamedKey::PlaySpeedDown),
        BevyKey::PlaySpeedReset => Key::Named(NamedKey::PlaySpeedReset),
        BevyKey::PlaySpeedUp => Key::Named(NamedKey::PlaySpeedUp),
        BevyKey::RandomToggle => Key::Named(NamedKey::RandomToggle),
        BevyKey::RcLowBattery => Key::Named(NamedKey::RcLowBattery),
        BevyKey::RecordSpeedNext => Key::Named(NamedKey::RecordSpeedNext),
        BevyKey::RfBypass => Key::Named(NamedKey::RfBypass),
        BevyKey::ScanChannelsToggle => Key::Named(NamedKey::ScanChannelsToggle),
        BevyKey::ScreenModeNext => Key::Named(NamedKey::ScreenModeNext),
        BevyKey::Settings => Key::Named(NamedKey::Settings),
        BevyKey::SplitScreenToggle => Key::Named(NamedKey::SplitScreenToggle),
        BevyKey::STBInput => Key::Named(NamedKey::STBInput),
        BevyKey::STBPower => Key::Named(NamedKey::STBPower),
        BevyKey::Subtitle => Key::Named(NamedKey::Subtitle),
        BevyKey::Teletext => Key::Named(NamedKey::Teletext),
        BevyKey::VideoModeNext => Key::Named(NamedKey::VideoModeNext),
        BevyKey::Wink => Key::Named(NamedKey::Wink),
        BevyKey::ZoomToggle => Key::Named(NamedKey::ZoomToggle),
        BevyKey::F1 => Key::Named(NamedKey::F1),
        BevyKey::F2 => Key::Named(NamedKey::F2),
        BevyKey::F3 => Key::Named(NamedKey::F3),
        BevyKey::F4 => Key::Named(NamedKey::F4),
        BevyKey::F5 => Key::Named(NamedKey::F5),
        BevyKey::F6 => Key::Named(NamedKey::F6),
        BevyKey::F7 => Key::Named(NamedKey::F7),
        BevyKey::F8 => Key::Named(NamedKey::F8),
        BevyKey::F9 => Key::Named(NamedKey::F9),
        BevyKey::F10 => Key::Named(NamedKey::F10),
        BevyKey::F11 => Key::Named(NamedKey::F11),
        BevyKey::F12 => Key::Named(NamedKey::F12),
        BevyKey::F13 => Key::Named(NamedKey::F13),
        BevyKey::F14 => Key::Named(NamedKey::F14),
        BevyKey::F15 => Key::Named(NamedKey::F15),
        BevyKey::F16 => Key::Named(NamedKey::F16),
        BevyKey::F17 => Key::Named(NamedKey::F17),
        BevyKey::F18 => Key::Named(NamedKey::F18),
        BevyKey::F19 => Key::Named(NamedKey::F19),
        BevyKey::F20 => Key::Named(NamedKey::F20),
        BevyKey::F21 => Key::Named(NamedKey::F21),
        BevyKey::F22 => Key::Named(NamedKey::F22),
        BevyKey::F23 => Key::Named(NamedKey::F23),
        BevyKey::F24 => Key::Named(NamedKey::F24),
        BevyKey::F25 => Key::Named(NamedKey::F25),
        BevyKey::F26 => Key::Named(NamedKey::F26),
        BevyKey::F27 => Key::Named(NamedKey::F27),
        BevyKey::F28 => Key::Named(NamedKey::F28),
        BevyKey::F29 => Key::Named(NamedKey::F29),
        BevyKey::F30 => Key::Named(NamedKey::F30),
        BevyKey::F31 => Key::Named(NamedKey::F31),
        BevyKey::F32 => Key::Named(NamedKey::F32),
        BevyKey::F33 => Key::Named(NamedKey::F33),
        BevyKey::F34 => Key::Named(NamedKey::F34),
        BevyKey::F35 => Key::Named(NamedKey::F35),
        _ => Key::Named(NamedKey::Unidentified),
    }
}

fn bevy_key_code_to_blitz_code(key_code: &BevyKeyCode) -> Code {
    match key_code {
        BevyKeyCode::Unidentified(_) => Code::Unidentified,
        BevyKeyCode::Backquote => Code::Backquote,
        BevyKeyCode::Backslash => Code::Backslash,
        BevyKeyCode::BracketLeft => Code::BracketLeft,
        BevyKeyCode::BracketRight => Code::BracketRight,
        BevyKeyCode::Comma => Code::Comma,
        BevyKeyCode::Digit0 => Code::Digit0,
        BevyKeyCode::Digit1 => Code::Digit1,
        BevyKeyCode::Digit2 => Code::Digit2,
        BevyKeyCode::Digit3 => Code::Digit3,
        BevyKeyCode::Digit4 => Code::Digit4,
        BevyKeyCode::Digit5 => Code::Digit5,
        BevyKeyCode::Digit6 => Code::Digit6,
        BevyKeyCode::Digit7 => Code::Digit7,
        BevyKeyCode::Digit8 => Code::Digit8,
        BevyKeyCode::Digit9 => Code::Digit9,
        BevyKeyCode::Equal => Code::Equal,
        BevyKeyCode::IntlBackslash => Code::IntlBackslash,
        BevyKeyCode::IntlRo => Code::IntlRo,
        BevyKeyCode::IntlYen => Code::IntlYen,
        BevyKeyCode::KeyA => Code::KeyA,
        BevyKeyCode::KeyB => Code::KeyB,
        BevyKeyCode::KeyC => Code::KeyC,
        BevyKeyCode::KeyD => Code::KeyD,
        BevyKeyCode::KeyE => Code::KeyE,
        BevyKeyCode::KeyF => Code::KeyF,
        BevyKeyCode::KeyG => Code::KeyG,
        BevyKeyCode::KeyH => Code::KeyH,
        BevyKeyCode::KeyI => Code::KeyI,
        BevyKeyCode::KeyJ => Code::KeyJ,
        BevyKeyCode::KeyK => Code::KeyK,
        BevyKeyCode::KeyL => Code::KeyL,
        BevyKeyCode::KeyM => Code::KeyM,
        BevyKeyCode::KeyN => Code::KeyN,
        BevyKeyCode::KeyO => Code::KeyO,
        BevyKeyCode::KeyP => Code::KeyP,
        BevyKeyCode::KeyQ => Code::KeyQ,
        BevyKeyCode::KeyR => Code::KeyR,
        BevyKeyCode::KeyS => Code::KeyS,
        BevyKeyCode::KeyT => Code::KeyT,
        BevyKeyCode::KeyU => Code::KeyU,
        BevyKeyCode::KeyV => Code::KeyV,
        BevyKeyCode::KeyW => Code::KeyW,
        BevyKeyCode::KeyX => Code::KeyX,
        BevyKeyCode::KeyY => Code::KeyY,
        BevyKeyCode::KeyZ => Code::KeyZ,
        BevyKeyCode::Minus => Code::Minus,
        BevyKeyCode::Period => Code::Period,
        BevyKeyCode::Quote => Code::Quote,
        BevyKeyCode::Semicolon => Code::Semicolon,
        BevyKeyCode::Slash => Code::Slash,
        BevyKeyCode::AltLeft => Code::AltLeft,
        BevyKeyCode::AltRight => Code::AltRight,
        BevyKeyCode::Backspace => Code::Backspace,
        BevyKeyCode::CapsLock => Code::CapsLock,
        BevyKeyCode::ContextMenu => Code::ContextMenu,
        BevyKeyCode::ControlLeft => Code::ControlLeft,
        BevyKeyCode::ControlRight => Code::ControlRight,
        BevyKeyCode::Enter => Code::Enter,
        BevyKeyCode::SuperLeft => Code::MetaLeft,
        BevyKeyCode::SuperRight => Code::MetaRight,
        BevyKeyCode::ShiftLeft => Code::ShiftLeft,
        BevyKeyCode::ShiftRight => Code::ShiftRight,
        BevyKeyCode::Space => Code::Space,
        BevyKeyCode::Tab => Code::Tab,
        BevyKeyCode::Convert => Code::Convert,
        BevyKeyCode::KanaMode => Code::KanaMode,
        BevyKeyCode::Lang1 => Code::Lang1,
        BevyKeyCode::Lang2 => Code::Lang2,
        BevyKeyCode::Lang3 => Code::Lang3,
        BevyKeyCode::Lang4 => Code::Lang4,
        BevyKeyCode::Lang5 => Code::Lang5,
        BevyKeyCode::NonConvert => Code::NonConvert,
        BevyKeyCode::Delete => Code::Delete,
        BevyKeyCode::End => Code::End,
        BevyKeyCode::Help => Code::Help,
        BevyKeyCode::Home => Code::Home,
        BevyKeyCode::Insert => Code::Insert,
        BevyKeyCode::PageDown => Code::PageDown,
        BevyKeyCode::PageUp => Code::PageUp,
        BevyKeyCode::ArrowDown => Code::ArrowDown,
        BevyKeyCode::ArrowLeft => Code::ArrowLeft,
        BevyKeyCode::ArrowRight => Code::ArrowRight,
        BevyKeyCode::ArrowUp => Code::ArrowUp,
        BevyKeyCode::NumLock => Code::NumLock,
        BevyKeyCode::Numpad0 => Code::Numpad0,
        BevyKeyCode::Numpad1 => Code::Numpad1,
        BevyKeyCode::Numpad2 => Code::Numpad2,
        BevyKeyCode::Numpad3 => Code::Numpad3,
        BevyKeyCode::Numpad4 => Code::Numpad4,
        BevyKeyCode::Numpad5 => Code::Numpad5,
        BevyKeyCode::Numpad6 => Code::Numpad6,
        BevyKeyCode::Numpad7 => Code::Numpad7,
        BevyKeyCode::Numpad8 => Code::Numpad8,
        BevyKeyCode::Numpad9 => Code::Numpad9,
        BevyKeyCode::NumpadAdd => Code::NumpadAdd,
        BevyKeyCode::NumpadBackspace => Code::NumpadBackspace,
        BevyKeyCode::NumpadClear => Code::NumpadClear,
        BevyKeyCode::NumpadClearEntry => Code::NumpadClearEntry,
        BevyKeyCode::NumpadComma => Code::NumpadComma,
        BevyKeyCode::NumpadDecimal => Code::NumpadDecimal,
        BevyKeyCode::NumpadDivide => Code::NumpadDivide,
        BevyKeyCode::NumpadEnter => Code::NumpadEnter,
        BevyKeyCode::NumpadEqual => Code::NumpadEqual,
        BevyKeyCode::NumpadHash => Code::NumpadHash,
        BevyKeyCode::NumpadMemoryAdd => Code::NumpadMemoryAdd,
        BevyKeyCode::NumpadMemoryClear => Code::NumpadMemoryClear,
        BevyKeyCode::NumpadMemoryRecall => Code::NumpadMemoryRecall,
        BevyKeyCode::NumpadMemoryStore => Code::NumpadMemoryStore,
        BevyKeyCode::NumpadMemorySubtract => Code::NumpadMemorySubtract,
        BevyKeyCode::NumpadMultiply => Code::NumpadMultiply,
        BevyKeyCode::NumpadParenLeft => Code::NumpadParenLeft,
        BevyKeyCode::NumpadParenRight => Code::NumpadParenRight,
        BevyKeyCode::NumpadStar => Code::NumpadStar,
        BevyKeyCode::NumpadSubtract => Code::NumpadSubtract,
        BevyKeyCode::Escape => Code::Escape,
        BevyKeyCode::Fn => Code::Fn,
        BevyKeyCode::FnLock => Code::FnLock,
        BevyKeyCode::PrintScreen => Code::PrintScreen,
        BevyKeyCode::ScrollLock => Code::ScrollLock,
        BevyKeyCode::Pause => Code::Pause,
        BevyKeyCode::BrowserBack => Code::BrowserBack,
        BevyKeyCode::BrowserFavorites => Code::BrowserFavorites,
        BevyKeyCode::BrowserForward => Code::BrowserForward,
        BevyKeyCode::BrowserHome => Code::BrowserHome,
        BevyKeyCode::BrowserRefresh => Code::BrowserRefresh,
        BevyKeyCode::BrowserSearch => Code::BrowserSearch,
        BevyKeyCode::BrowserStop => Code::BrowserStop,
        BevyKeyCode::Eject => Code::Eject,
        BevyKeyCode::LaunchApp1 => Code::LaunchApp1,
        BevyKeyCode::LaunchApp2 => Code::LaunchApp2,
        BevyKeyCode::LaunchMail => Code::LaunchMail,
        BevyKeyCode::MediaPlayPause => Code::MediaPlayPause,
        BevyKeyCode::MediaSelect => Code::MediaSelect,
        BevyKeyCode::MediaStop => Code::MediaStop,
        BevyKeyCode::MediaTrackNext => Code::MediaTrackNext,
        BevyKeyCode::MediaTrackPrevious => Code::MediaTrackPrevious,
        BevyKeyCode::Power => Code::Power,
        BevyKeyCode::Sleep => Code::Sleep,
        BevyKeyCode::AudioVolumeDown => Code::AudioVolumeDown,
        BevyKeyCode::AudioVolumeMute => Code::AudioVolumeMute,
        BevyKeyCode::AudioVolumeUp => Code::AudioVolumeUp,
        BevyKeyCode::WakeUp => Code::WakeUp,
        BevyKeyCode::Meta => Code::Hyper,
        BevyKeyCode::Hyper => Code::Super,
        BevyKeyCode::Turbo => Code::Turbo,
        BevyKeyCode::Abort => Code::Abort,
        BevyKeyCode::Resume => Code::Resume,
        BevyKeyCode::Suspend => Code::Suspend,
        BevyKeyCode::Again => Code::Again,
        BevyKeyCode::Copy => Code::Copy,
        BevyKeyCode::Cut => Code::Cut,
        BevyKeyCode::Find => Code::Find,
        BevyKeyCode::Open => Code::Open,
        BevyKeyCode::Paste => Code::Paste,
        BevyKeyCode::Props => Code::Props,
        BevyKeyCode::Select => Code::Select,
        BevyKeyCode::Undo => Code::Undo,
        BevyKeyCode::Hiragana => Code::Hiragana,
        BevyKeyCode::Katakana => Code::Katakana,
        BevyKeyCode::F1 => Code::F1,
        BevyKeyCode::F2 => Code::F2,
        BevyKeyCode::F3 => Code::F3,
        BevyKeyCode::F4 => Code::F4,
        BevyKeyCode::F5 => Code::F5,
        BevyKeyCode::F6 => Code::F6,
        BevyKeyCode::F7 => Code::F7,
        BevyKeyCode::F8 => Code::F8,
        BevyKeyCode::F9 => Code::F9,
        BevyKeyCode::F10 => Code::F10,
        BevyKeyCode::F11 => Code::F11,
        BevyKeyCode::F12 => Code::F12,
        BevyKeyCode::F13 => Code::F13,
        BevyKeyCode::F14 => Code::F14,
        BevyKeyCode::F15 => Code::F15,
        BevyKeyCode::F16 => Code::F16,
        BevyKeyCode::F17 => Code::F17,
        BevyKeyCode::F18 => Code::F18,
        BevyKeyCode::F19 => Code::F19,
        BevyKeyCode::F20 => Code::F20,
        BevyKeyCode::F21 => Code::F21,
        BevyKeyCode::F22 => Code::F22,
        BevyKeyCode::F23 => Code::F23,
        BevyKeyCode::F24 => Code::F24,
        BevyKeyCode::F25 => Code::F25,
        BevyKeyCode::F26 => Code::F26,
        BevyKeyCode::F27 => Code::F27,
        BevyKeyCode::F28 => Code::F28,
        BevyKeyCode::F29 => Code::F29,
        BevyKeyCode::F30 => Code::F30,
        BevyKeyCode::F31 => Code::F31,
        BevyKeyCode::F32 => Code::F32,
        BevyKeyCode::F33 => Code::F33,
        BevyKeyCode::F34 => Code::F34,
        BevyKeyCode::F35 => Code::F35,
    }
}
