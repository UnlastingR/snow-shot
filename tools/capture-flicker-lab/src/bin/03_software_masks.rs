use capture_flicker_lab::{Renderer, run_slint_case};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_slint_case(
        Renderer::Software,
        "03 · Software renderer · 四块动态蒙层",
        true,
    )
}
