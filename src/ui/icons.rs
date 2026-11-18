//! 矢量图标：全部用 Painter 几何绘制，不依赖任何字体字形，任何系统上都不会显示成方框。
//! 坐标系为 16×16 网格，自动缩放到目标矩形。
use eframe::egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, pos2};
use std::f32::consts::TAU;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Plus,
    Play,
    Pause,
    Trash,
    Settings,
    Search,
    Folder,
    External,
    Copy,
    Download,
    Activity,
    Check,
    Alert,
    Clock,
    PauseCircle,
    X,
    ArrowDown,
    Refresh,
    Inbox,
}

#[cfg(test)]
pub const ALL: [Icon; 19] = [
    Icon::Plus,
    Icon::Play,
    Icon::Pause,
    Icon::Trash,
    Icon::Settings,
    Icon::Search,
    Icon::Folder,
    Icon::External,
    Icon::Copy,
    Icon::Download,
    Icon::Activity,
    Icon::Check,
    Icon::Alert,
    Icon::Clock,
    Icon::PauseCircle,
    Icon::X,
    Icon::ArrowDown,
    Icon::Refresh,
    Icon::Inbox,
];

struct Pen<'a> {
    p: &'a Painter,
    o: Pos2,
    s: f32,
    stroke: Stroke,
    color: Color32,
}

impl Pen<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.o.x + x * self.s, self.o.y + y * self.s)
    }
    fn line(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::line(v, self.stroke));
    }
    fn closed(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::closed_line(v, self.stroke));
    }
    fn fill(&self, pts: &[(f32, f32)]) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::convex_polygon(v, self.color, Stroke::NONE));
    }
    fn circle(&self, x: f32, y: f32, r: f32) {
        self.p.circle_stroke(self.pt(x, y), r * self.s, self.stroke);
    }
    fn dot(&self, x: f32, y: f32, r: f32) {
        self.p.circle_filled(self.pt(x, y), r * self.s, self.color);
    }
    fn round_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, filled: bool) {
        let rect = Rect::from_min_max(self.pt(x0, y0), self.pt(x1, y1));
        let r = (r * self.s) as u8;
        if filled {
            self.p.rect_filled(rect, r, self.color);
        } else {
            self.p.rect_stroke(rect, r, self.stroke, eframe::egui::StrokeKind::Middle);
        }
    }
    /// 圆弧，角度单位为圈（0..1），0 为 3 点钟方向，顺时针
    fn arc(&self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Vec<(f32, f32)> {
        let n = 24;
        (0..=n)
            .map(|i| {
                let a = (a0 + (a1 - a0) * i as f32 / n as f32) * TAU;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    }
}

/// 在 rect 中绘制图标（rect 建议为正方形）
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = rect.width().min(rect.height());
    let s = side / 16.0;
    let o = pos2(rect.center().x - side / 2.0, rect.center().y - side / 2.0);
    let pen = Pen { p, o, s, stroke: Stroke::new((1.5 * s).max(1.1), color), color };
    match icon {
        Icon::Plus => {
            pen.line(&[(8.0, 3.0), (8.0, 13.0)]);
            pen.line(&[(3.0, 8.0), (13.0, 8.0)]);
        }
        Icon::Play => pen.fill(&[(4.5, 2.8), (13.2, 8.0), (4.5, 13.2)]),
        Icon::Pause => {
            pen.round_rect(4.0, 3.0, 6.6, 13.0, 0.8, true);
            pen.round_rect(9.4, 3.0, 12.0, 13.0, 0.8, true);
        }
        Icon::Trash => {
            pen.line(&[(2.5, 4.5), (13.5, 4.5)]);
            pen.line(&[(6.0, 4.5), (6.0, 2.5), (10.0, 2.5), (10.0, 4.5)]);
            pen.line(&[(3.8, 4.5), (4.6, 13.5), (11.4, 13.5), (12.2, 4.5)]);
            pen.line(&[(6.7, 7.0), (6.7, 11.0)]);
            pen.line(&[(9.3, 7.0), (9.3, 11.0)]);
        }
        Icon::Settings => {
            pen.circle(8.0, 8.0, 2.1);
            pen.circle(8.0, 8.0, 4.6);
            let thick = Stroke::new(pen.stroke.width * 1.6, color);
            for k in 0..8 {
                let a = k as f32 / 8.0 * TAU;
                let (c, sn) = (a.cos(), a.sin());
                p.line_segment([pen.pt(8.0 + 4.9 * c, 8.0 + 4.9 * sn), pen.pt(8.0 + 6.6 * c, 8.0 + 6.6 * sn)], thick);
            }
        }
        Icon::Search => {
            pen.circle(7.0, 7.0, 4.5);
            pen.line(&[(10.4, 10.4), (13.6, 13.6)]);
        }
        Icon::Folder => pen.closed(&[(2.0, 3.8), (6.2, 3.8), (7.7, 5.4), (14.0, 5.4), (14.0, 12.8), (2.0, 12.8)]),
        Icon::External => {
            pen.line(&[(9.5, 2.5), (13.5, 2.5), (13.5, 6.5)]);
            pen.line(&[(13.5, 2.5), (7.5, 8.5)]);
            pen.line(&[(11.5, 9.5), (11.5, 13.5), (2.5, 13.5), (2.5, 4.5), (6.5, 4.5)]);
        }
        Icon::Copy => {
            pen.round_rect(5.5, 5.5, 13.5, 13.5, 1.5, false);
            pen.line(&[(10.5, 5.5), (10.5, 2.5), (2.5, 2.5), (2.5, 10.5), (5.5, 10.5)]);
        }
        Icon::Download => {
            pen.line(&[(8.0, 2.5), (8.0, 10.0)]);
            pen.line(&[(4.5, 6.8), (8.0, 10.3), (11.5, 6.8)]);
            pen.line(&[(2.8, 13.5), (13.2, 13.5)]);
        }
        Icon::Activity => pen.line(&[(1.5, 8.0), (4.5, 8.0), (6.5, 3.0), (9.5, 13.0), (11.5, 8.0), (14.5, 8.0)]),
        Icon::Check => {
            pen.circle(8.0, 8.0, 6.0);
            pen.line(&[(5.2, 8.2), (7.2, 10.2), (10.9, 6.2)]);
        }
        Icon::Alert => {
            pen.circle(8.0, 8.0, 6.0);
            pen.line(&[(8.0, 4.8), (8.0, 8.8)]);
            pen.dot(8.0, 11.1, 0.95);
        }
        Icon::Clock => {
            pen.circle(8.0, 8.0, 6.0);
            pen.line(&[(8.0, 4.6), (8.0, 8.0), (10.4, 9.5)]);
        }
        Icon::PauseCircle => {
            pen.circle(8.0, 8.0, 6.0);
            pen.line(&[(6.6, 5.6), (6.6, 10.4)]);
            pen.line(&[(9.4, 5.6), (9.4, 10.4)]);
        }
        Icon::X => {
            pen.line(&[(4.0, 4.0), (12.0, 12.0)]);
            pen.line(&[(12.0, 4.0), (4.0, 12.0)]);
        }
        Icon::ArrowDown => {
            pen.line(&[(8.0, 2.5), (8.0, 13.0)]);
            pen.line(&[(3.8, 8.8), (8.0, 13.0), (12.2, 8.8)]);
        }
        Icon::Refresh => {
            // 缺口在右侧的圆弧，箭头位于终点（右上）
            pen.line(&pen.arc(8.0, 8.0, 5.2, 0.1, 0.9));
            pen.line(&[(12.15, 2.1), (12.2, 4.95), (9.5, 4.0)]);
        }
        Icon::Inbox => {
            pen.closed(&[(2.0, 9.0), (4.5, 3.0), (11.5, 3.0), (14.0, 9.0), (14.0, 13.5), (2.0, 13.5)]);
            pen.line(&[(2.0, 9.0), (5.5, 9.0), (6.5, 10.8), (9.5, 10.8), (10.5, 9.0), (14.0, 9.0)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{self, Id, LayerId, Order, vec2};

    /// 每个图标都必须产生几何图形，且全部落在自身方框内（允许描边外扩 2px）
    #[test]
    fn every_icon_draws_inside_its_box() {
        let ctx = egui::Context::default();
        for (i, icon) in ALL.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(100.0, 100.0), vec2(16.0, 16.0));
            let out = ctx.run_ui(Default::default(), |ui| {
                let painter = ui.ctx().layer_painter(LayerId::new(Order::Foreground, Id::unique(("icon", i))));
                paint(&painter, rect, *icon, Color32::WHITE);
            });
            let shapes: Vec<_> = out.shapes.iter().filter(|s| s.shape.visual_bounding_rect().is_positive()).collect();
            assert!(!shapes.is_empty(), "{icon:?} 没有绘制任何图形");
            for s in shapes {
                let b = s.shape.visual_bounding_rect();
                assert!(rect.expand(2.0).contains_rect(b), "{icon:?} 超出边界: {b:?}");
            }
        }
    }
}
