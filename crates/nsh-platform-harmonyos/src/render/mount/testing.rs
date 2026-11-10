//! Native geometry for the computer-side test agent. Selectors come from
//! the model; positions come from the views that have actually been mounted.

use super::Mounter;
use nsh_dev::protocol::SceneQuery;
use serde_json::{Value, json};

impl Mounter {
    fn focused_input(&self) -> Option<(std::rc::Rc<crate::input::editor::Input>, u32)> {
        self.slots.iter().flatten().find_map(|node| {
            let input = node.input.borrow().as_ref()?.clone();
            input.is_focused().then_some((input, node.id))
        })
    }

    pub(crate) fn test_type(&self, text: &str) -> Result<(), String> {
        if let Some((input, _)) = self.focused_input() {
            input.test_type(text);
            self.flush_arkui().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn test_key(&self, kind: &str) -> Result<(), String> {
        if let Some((input, _)) = self.focused_input() {
            input.test_key(kind);
            self.flush_arkui().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn test_paste(&self, content: nsh_core::PasteEvent) -> Result<(), String> {
        if let Some((input, _)) = self.focused_input() {
            input.test_paste(content).map_err(|e| e.to_string())?;
            self.flush_arkui().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn test_query(&self, query: SceneQuery) -> Result<Value, String> {
        match query {
            SceneQuery::Bounds { id: u32::MAX } => {
                let bounds = (|| {
                    let root = self.node(nsh_core::ROOT_ID).ok()?;
                    let frame = root.frame.get();
                    if frame.width <= 0.0 || frame.height <= 0.0 {
                        return None;
                    }
                    let origin = self
                        .mount
                        .parent
                        .upgrade()
                        .and_then(|parent| parent.borrow().layout_position_in_screen().ok())
                        .or_else(|| root.mounted().borrow().layout_position_in_screen().ok())?;
                    let scale = self.scale.get();
                    Some([
                        origin.x as f32,
                        origin.y as f32,
                        frame.width * scale,
                        frame.height * scale,
                    ])
                })();
                Ok(json!(bounds))
            }
            SceneQuery::Bounds { id } => Ok(json!(self.test_bounds(id))),
            SceneQuery::TextBounds { id, start, end } => {
                let bounds = (|| {
                    let node = self.node(id).ok()?;
                    let layout = node.raster.borrow();
                    let text = layout
                        .as_ref()?
                        .0
                        .downcast_ref::<crate::text::typography::NativeText>()?;
                    let region = text
                        .selection
                        .as_ref()
                        .and_then(|selection| {
                            selection
                                .rectangles(selection.clusters(start..end))
                                .next()
                                .map(|r| r.bounds)
                        })
                        .or_else(|| {
                            text.links
                                .iter()
                                .find(|(link, _)| link.start <= start && link.end >= end)
                                .and_then(|(_, boxes)| boxes.first().copied())
                        })?;
                    self.test_rect(id, region)
                })();
                Ok(json!(bounds))
            }
            SceneQuery::InputText { id: u32::MAX } => {
                Ok(json!(self.focused_input().map(|(_, id)| id.to_string())))
            }
            SceneQuery::InputText { id } => {
                Ok(json!(self.node(id).ok().and_then(|node| {
                    node.input.borrow().as_ref().map(|input| input.test_text())
                })))
            }
            SceneQuery::Animating => Ok(json!(
                self.animator.test_animating()
                    || self.shaders.animating()
                    || self.leaving_nodes.get() > 0
                    || self.slots.iter().flatten().any(|node| node
                        .scroll
                        .borrow()
                        .as_ref()
                        .is_some_and(|scroll| scroll.test_animating())
                        || node
                            .screen
                            .borrow()
                            .as_ref()
                            .is_some_and(|screen| screen.test_animating())
                        || node
                            .modal
                            .borrow()
                            .as_ref()
                            .is_some_and(|modal| modal.test_animating()))
            )),
            SceneQuery::DialogPoint { button } => {
                let root = self.node(nsh_core::ROOT_ID).map_err(|e| e.to_string())?;
                let origin = self
                    .mount
                    .parent
                    .upgrade()
                    .and_then(|parent| parent.borrow().layout_position_in_screen().ok())
                    .or_else(|| root.mounted().borrow().layout_position_in_screen().ok())
                    .ok_or("root node is not laid out on screen")?;
                Ok(json!(self.dialogs.test_point(
                    button.as_deref(),
                    (origin.x, origin.y),
                    self.scale.get()
                )?))
            }
            _ => Err("this query belongs to the test model".into()),
        }
    }

    fn test_bounds(&self, id: u32) -> Option<[f32; 4]> {
        let node = self.node(id).ok()?;
        if node.pending_delete.get() || node.desired_visibility() != 0 {
            return None;
        }
        let frame = node.frame.get();
        self.test_rect(id, [0.0, 0.0, frame.width, frame.height])
    }

    fn test_rect(&self, id: u32, rect: [f32; 4]) -> Option<[f32; 4]> {
        let node = self.node(id).ok()?;
        let root = self.node(nsh_core::ROOT_ID).ok()?;
        let mut plane = node.clone();
        while plane.id != nsh_core::ROOT_ID && plane.kind != nsh_core::ViewKind::Modal {
            let parent = plane.parent_node.borrow().upgrade()?;
            plane = parent;
        }
        let mut matrix = node.input_in_root(&plane).ok()??;
        if plane.kind == nsh_core::ViewKind::Modal {
            let position = plane.mounted().borrow().layout_position_in_screen().ok()?;
            let origin = root.mounted().borrow().layout_position_in_screen().ok()?;
            use nsh_core::{Matrix, TransformKind};
            matrix = matrix
                .mul(&Matrix::of(
                    TransformKind::TranslateX,
                    (position.x - origin.x) as f64 / self.scale.get() as f64,
                ))
                .mul(&Matrix::of(
                    TransformKind::TranslateY,
                    (position.y - origin.y) as f64 / self.scale.get() as f64,
                ));
        }
        let [x, y, right, bottom] = crate::render::composition::geometry::Bounds(rect)
            .transformed(matrix)?
            .0;
        Some([x, y, right - x, bottom - y])
    }
}
