//! SynCode UI — gpui / gpui-component 验证 spike。
//!
//! 目标 (架构 §12 待验证项): 确认 gpui + gpui-component 在 Windows 上**能编、能跑、能渲染**一个窗口,
//! 再决定往里投真正的 agent UI。代码照搬 gpui-component 的 hello_world 最小窗口 (上层是 `Root`)。

use gpui::*;
use gpui_component::{button::*, *};

pub struct SpikeView;

impl Render for SpikeView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_4()
            .size_full()
            .items_center()
            .justify_center()
            .child("SynCode UI — gpui spike on Windows")
            .child(
                Button::new("go")
                    .primary()
                    .label("It renders!")
                    .on_click(|_, _, _| println!("clicked — gpui event loop works")),
            )
    }
}

fn main() {
    gpui_platform::application().run(move |cx| {
        // gpui-component 任何特性使用前必须先 init。
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|_| SpikeView);
                // 窗口第一层必须是 Root。
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
