fn main() {
    slint_build::compile("ui/app-window.slint")
        .expect("failed to compile Snow Shot native UI");
}
