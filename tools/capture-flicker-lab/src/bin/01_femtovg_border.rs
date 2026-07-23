use capture_flicker_lab::{Renderer, run_slint_case};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_slint_case(
        Renderer::FemtoVg,
        "01 · FemtoVG / OpenGL · 仅移动边框",
        false,
    )
}
