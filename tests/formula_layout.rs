#[test]
fn mathjax_generates_self_contained_svg_for_display_formula() {
    let svg = mathjax_svg_rs::render_tex(
        r"\int_0^1 x^2\,dx = \frac{1}{3}",
        &mathjax_svg_rs::Options {
            font_size: 19.0,
            horizontal_align: mathjax_svg_rs::HorizontalAlign::Center,
        },
    )
    .expect("MathJax should render valid TeX");

    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("viewBox="));
    assert!(svg.ends_with("</svg>"));
    assert!(!svg.contains("<script"));
}
