// Fullscreen background: solid color or wallpaper image, with global window
// opacity (see-through), aspect-preserving "cover" fit, and rounded corners.

struct BgParams {
    color: vec4<f32>,
    screen: vec2<f32>,
    img_size: vec2<f32>,
    opacity: f32,       // global window opacity 0..1
    has_image: f32,     // 0 or 1
    radius: f32,        // rounded-corner radius (px)
    _pad: f32,
};
@group(0) @binding(0) var<uniform> p: BgParams;
@group(0) @binding(1) var img_tex: texture_2d<f32>;
@group(0) @binding(2) var img_samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle.
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0),
    );
    var uv = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0), vec2<f32>(2.0, 1.0), vec2<f32>(0.0, -1.0),
    );
    var out: VsOut;
    out.clip = vec4<f32>(pos[vi], 0.0, 1.0);
    out.uv = uv[vi];
    return out;
}

// Signed distance to a rounded box centered at origin, half-size hb, radius r.
fn sd_round_box(pt: vec2<f32>, hb: vec2<f32>, r: f32) -> f32 {
    let q = abs(pt) - hb + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Layer 1: solid background color, full size.
    var col = p.color.rgb;

    // Layer 2: wallpaper on top, "object-fit: cover" (aspect-preserving, cropped),
    // composited over the color using the image's own alpha.
    if (p.has_image > 0.5) {
        let sa = p.screen.x / p.screen.y;
        let ia = p.img_size.x / p.img_size.y;
        var uv = in.uv;
        if (sa > ia) {
            uv.y = 0.5 + (in.uv.y - 0.5) * (ia / sa);
        } else {
            uv.x = 0.5 + (in.uv.x - 0.5) * (sa / ia);
        }
        let img = textureSample(img_tex, img_samp, uv);
        col = mix(col, img.rgb, img.a);
    }

    // Rounded-corner clip matching the RGB border's outer edge.
    var edge = 1.0;
    if (p.radius > 0.0) {
        let frag = in.uv * p.screen; // pixel pos (uv is 0..1 over screen)
        let sd = sd_round_box(frag - p.screen * 0.5, p.screen * 0.5, p.radius);
        edge = 1.0 - smoothstep(-1.0, 0.0, sd); // hard at the outer edge, 1px AA
    }

    // Global window opacity. Premultiplied alpha for the transparent surface.
    let a = p.opacity * edge;
    return vec4<f32>(col * a, a);
}
