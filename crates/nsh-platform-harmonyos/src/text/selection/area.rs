//! The core selection model over mounted text recordings, in an area's coordinates.
use std::{
    rc::{Rc, Weak},
    sync::Arc,
};

use nsh_core::{
    Bounds, Matrix, Point, Points, Size,
    selection::{KeptRows, SelectionModel, SelectionTexts, reading_order, reorder},
};

use crate::{
    render::{
        geometry::{Rect, local_point},
        node::{Node, Result},
    },
    text::typography::NativeText,
};

struct Item {
    node: Rc<Node>,
    text: Arc<NativeText>,
    frame: Bounds<Points>,
    matrix: Matrix,
    content: Rect,
    continues: bool,
}
pub(crate) struct Texts(Vec<Item>);
type HandleEdge = (nsh_core::selection::Handle, (f32, f32, f32));
impl SelectionTexts for Texts {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn content(&self, i: usize) -> &str {
        &self.0[i].text.text
    }
    fn frame(&self, i: usize) -> Bounds<Points> {
        self.0[i].frame
    }
    fn key(&self, i: usize) -> u64 {
        u64::from(self.0[i].node.id)
    }
    fn continues(&self, i: usize) -> bool {
        self.0[i].continues
    }
    fn offset_at(&self, i: usize, point: Point<Points>) -> u32 {
        let item = &self.0[i];
        let Some(point) = local_point(&item.matrix.0, (point.x.0, point.y.0)) else {
            return 0;
        };
        item.text
            .selection
            .as_ref()
            .and_then(|l| {
                l.boundary((point.0 - item.content.x, point.1 - item.content.y))
                    .map(|b| l.utf16(b))
            })
            .unwrap_or(0)
    }
}
impl Texts {
    fn collect(area: &Rc<Node>) -> Result<Self> {
        fn walk(node: &Rc<Node>, matrix: Matrix, items: &mut Vec<Item>) -> Result<()> {
            if !node.input_visible() {
                return Ok(());
            }
            let content = node.content.get();
            if let Some(text) = node
                .raster
                .borrow()
                .as_ref()
                .and_then(|t| t.0.clone().downcast::<NativeText>().ok())
                && text.selection.is_some()
                && let Some(bounds) = crate::render::composition::geometry::Bounds([
                    content.x,
                    content.y,
                    content.x + content.width,
                    content.y + content.height,
                ])
                .transformed(matrix)
            {
                let [x, y, right, bottom] = bounds.0;
                items.push(Item {
                    node: node.clone(),
                    text,
                    frame: Bounds::new(
                        Point::new(Points(x), Points(y)),
                        Size::new(Points(right - x), Points(bottom - y)),
                    ),
                    matrix,
                    content,
                    continues: node.interaction.continues_text(),
                });
            }
            let start = items.len();
            let mut runs = Vec::new();
            let plane = node.input_children_transform()?.mul(&matrix);
            for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
                let from = items.len();
                walk(&child, child.input_in_plane(&plane)?, items)?;
                runs.push((child.frame.get().into(), from..items.len()));
            }
            if let Some(order) = reading_order(&runs) {
                reorder(items, start, &order);
            }
            Ok(())
        }
        let mut items = Vec::new();
        walk(area, Matrix::IDENTITY, &mut items)?;
        Ok(Self(items))
    }
}

pub(crate) struct Area {
    root: Weak<Node>,
    pub(crate) model: SelectionModel,
    members: Vec<Weak<Node>>,
    kept: KeptRows,
    reported: String,
}
impl Area {
    pub(crate) fn root(node: &Rc<Node>) -> Rc<Node> {
        let mut area = node.clone();
        let mut parent = Some(node.clone());
        while let Some(node) = parent {
            if node.interaction.selectable_view() {
                area = node.clone();
            }
            parent = node.parent_node.borrow().upgrade();
        }
        area
    }
    pub(crate) fn new(root: &Rc<Node>) -> Self {
        Self {
            root: Rc::downgrade(root),
            model: SelectionModel::new(),
            members: Vec::new(),
            kept: KeptRows::default(),
            reported: String::new(),
        }
    }
    pub(crate) fn update(
        &mut self,
        owner: &Node,
        action: impl FnOnce(&mut SelectionModel, &Texts, Matrix),
    ) -> Result<()> {
        let Some(root) = self.root.upgrade() else {
            return Ok(());
        };
        let texts = Texts::collect(&root)?;
        self.model.texts_changed(&texts);
        if let Some(matrix) = owner.input_in_root(&root)? {
            action(&mut self.model, &texts, matrix);
        }
        let ranges = self.model.ranges(&texts);
        for old in self.members.drain(..).filter_map(|n| n.upgrade()) {
            if !ranges
                .iter()
                .any(|(index, _)| texts.0[*index].node.id == old.id)
            {
                old.selection_clear_local()?;
            }
        }
        for (index, range) in ranges {
            let item = &texts.0[index];
            if let Some(layout) = &item.text.selection {
                let bounds = self.model.bounds().unwrap();
                let handles = [index == bounds.0.text, index == bounds.1.text];
                if item.node.selection.handles.replace(handles) != handles {
                    item.node.native.borrow().mark_dirty(
                        ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedRender,
                    )?;
                }
                item.node.selection_set(layout.clusters(range))?;
                self.members.push(Rc::downgrade(&item.node));
            }
        }
        if let Some(nodes) = self.kept.changed(&self.model) {
            owner
                .control
                .keep_rendered(nodes.into_iter().map(|id| id as u32).collect());
        }
        if !root.pending_delete.get()
            && root
                .interaction
                .mask()
                .contains(nsh_core::EventKind::SelectionChange)
        {
            let (text, start) = if self.model.is_empty() {
                (String::new(), 0)
            } else {
                (self.model.text(&texts), self.model.start_offset(&texts))
            };
            if text != self.reported {
                self.reported = text.clone();
                root.emit(
                    nsh_core::EventKind::SelectionChange,
                    nsh_core::EventPayload::Map(vec![
                        ("text", nsh_core::PayloadValue::Str(text)),
                        ("start", nsh_core::PayloadValue::Number(f64::from(start))),
                    ]),
                );
            }
        }
        Ok(())
    }
    pub(crate) fn text(&self) -> Result<String> {
        self.root.upgrade().map_or(Ok(String::new()), |root| {
            Ok(self.model.text(&Texts::collect(&root)?))
        })
    }
    pub(crate) fn clear(&mut self, owner: &Node) -> Result<()> {
        self.model.clear();
        for node in self.members.drain(..).filter_map(|n| n.upgrade()) {
            node.selection_clear_local()?;
        }
        if let Some(nodes) = self.kept.release() {
            owner
                .control
                .keep_rendered(nodes.into_iter().map(|id| id as u32).collect());
        }
        if !std::mem::take(&mut self.reported).is_empty()
            && let Some(root) = self.root.upgrade()
            && !root.pending_delete.get()
            && root
                .interaction
                .mask()
                .contains(nsh_core::EventKind::SelectionChange)
        {
            root.emit(
                nsh_core::EventKind::SelectionChange,
                nsh_core::EventPayload::Map(vec![
                    ("text", nsh_core::PayloadValue::Str(String::new())),
                    ("start", nsh_core::PayloadValue::Number(0.0)),
                ]),
            );
        }
        Ok(())
    }
    pub(crate) fn handles(&self, owner: &Node) -> Result<Vec<HandleEdge>> {
        let Some(root) = self.root.upgrade() else {
            return Ok(Vec::new());
        };
        let Some(owner_matrix) = owner.input_in_root(&root)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for node in self.members.iter().filter_map(Weak::upgrade) {
            let Some(matrix) = node.input_in_root(&root)? else {
                continue;
            };
            let Some(edges) = node.selection.geometry.borrow().handles else {
                continue;
            };
            let content = node.content.get();
            for (index, (x, top, bottom)) in edges.into_iter().enumerate() {
                if !node.selection.handles.get()[index] {
                    continue;
                }
                let top = project(matrix, (content.x + x, content.y + top));
                let bottom = project(matrix, (content.x + x, content.y + bottom));
                if let (Some(top), Some(bottom)) = (
                    local_point(&owner_matrix.0, (top.x.0, top.y.0)),
                    local_point(&owner_matrix.0, (bottom.x.0, bottom.y.0)),
                ) {
                    out.push((
                        if index == 0 {
                            nsh_core::selection::Handle::Start
                        } else {
                            nsh_core::selection::Handle::End
                        },
                        (bottom.0, top.1, bottom.1),
                    ));
                }
            }
        }
        Ok(out)
    }
}
pub(crate) fn project(matrix: Matrix, point: (f32, f32)) -> Point<Points> {
    let m = matrix.0;
    let (x, y) = (f64::from(point.0), f64::from(point.1));
    let w = x * m[3] + y * m[7] + m[15];
    Point::new(
        Points(((x * m[0] + y * m[4] + m[12]) / w) as f32),
        Points(((x * m[1] + y * m[5] + m[13]) / w) as f32),
    )
}
