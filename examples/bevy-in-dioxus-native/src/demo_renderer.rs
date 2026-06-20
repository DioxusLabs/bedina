use crate::bevy_renderer::BevyRenderer;
use crate::Color;
use anyrender::{PaintScene as _, RenderContext, ResourceId, Scene};
use blitz_dom::node::ComputedStyles;
use blitz_traits::events::UiEvent;
use dioxus_native::{DeviceHandle, Widget};
use std::sync::mpsc::{channel, Receiver, Sender};

pub enum DemoMessage {
    // Color in RGB format
    SetColor(Color),
}

enum DemoRendererState {
    Active(Box<BevyRenderer>),
    Suspended,
}

pub struct DemoWidget {
    state: DemoRendererState,
    start_time: std::time::Instant,
    tx: Sender<DemoMessage>,
    rx: Receiver<DemoMessage>,
    color: Color,
}

impl DemoWidget {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self::with_channel(tx, rx)
    }

    pub fn with_channel(tx: Sender<DemoMessage>, rx: Receiver<DemoMessage>) -> Self {
        Self {
            state: DemoRendererState::Suspended,
            start_time: std::time::Instant::now(),
            tx,
            rx,
            color: Color::WHITE,
        }
    }

    pub fn sender(&self) -> Sender<DemoMessage> {
        self.tx.clone()
    }

    fn process_messages(&mut self) {
        loop {
            match self.rx.try_recv() {
                Err(_) => return,
                Ok(msg) => match msg {
                    DemoMessage::SetColor(color) => self.color = color,
                },
            }
        }
    }

    fn render(
        &mut self,
        ctx: &mut dyn RenderContext,
        width: u32,
        height: u32,
    ) -> Option<ResourceId> {
        if width == 0 || height == 0 {
            return None;
        }
        let DemoRendererState::Active(state) = &mut self.state else {
            return None;
        };

        state.render(ctx, self.color.components, width, height, &self.start_time)
    }
}

impl Widget for DemoWidget {
    fn connected(&mut self) {}
    fn disconnected(&mut self) {}

    fn attribute_changed(&mut self, name: &str, old_value: Option<&str>, new_value: Option<&str>) {
        let _ = (name, old_value, new_value);
    }

    fn can_create_surfaces(&mut self, render_ctx: &mut dyn RenderContext) {
        if let Some(renderer_specific_context) = render_ctx.renderer_specific_context() {
            if let Ok(device_handle) = renderer_specific_context.downcast::<DeviceHandle>() {
                let active_state = BevyRenderer::new(&device_handle);
                self.state = DemoRendererState::Active(Box::new(active_state));
            } else {
                println!("WARNING: Running WGPU example with non-wgpu rendering backend");
            }
        } else {
            println!("WARNING: Rendering backend returned no context!");
        }
    }

    fn destroy_surfaces(&mut self) {
        self.state = DemoRendererState::Suspended;
    }

    fn handle_event(&mut self, event: &UiEvent) {
        let _ = event;
    }

    fn paint(
        &mut self,
        render_ctx: &mut dyn RenderContext,
        _styles: &ComputedStyles,
        width: u32,
        height: u32,
        _scale: f64,
    ) -> Scene {
        let mut scene = anyrender::Scene::new();

        use anyrender::PaintRef;
        use peniko::kurbo::{Affine, Rect};
        use peniko::{Fill, ImageBrush, ImageSampler};

        // if matches!(self.state, DemoRendererState::Suspended) {
        //     self.can_create_surfaces(render_ctx);
        // }

        self.process_messages();
        if let Some(resource_id) = self.render(render_ctx, width, height) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                PaintRef::Resource(ImageBrush {
                    image: resource_id,
                    sampler: ImageSampler::default(),
                }),
                None,
                &Rect::from_origin_size((0.0, 0.0), (width as f64, height as f64)),
            );
        } else {
            println!("WANRING: render returned None");
        }

        scene
    }
}
