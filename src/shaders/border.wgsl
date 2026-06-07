// Hyprland-style animated RGB border drawn as a rounded-rectangle ring on top
// of the scene. Hue cycles around the frame and rotates over time.

struct BorderParams {
    screen: vec2<f32>,
    width: f32,
    radius: f32,
    density: f32,   // how many rainbow cycles around the frame
    phase: f32,     // animation phase (time / speed), wraps each second
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> b: BorderParams;

struct VsOut { @builtin(position) clip: vec4<f32> };

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0),
    );
    var out: VsOut;
    out.clip = vec4<f32>(pos[vi], 0.0, 1.0);
    return out;
}

// Signed distance to a rounded box centered at origin, half-size hb, radius r.
fn sd_round_box(p: vec2<f32>, hb: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - hb + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn hsv2rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let k = vec3<f32>(5.0, 3.0, 1.0);
    let p = abs(fract(vec3<f32>(h, h, h) + k / 6.0) * 6.0 - vec3<f32>(3.0, 3.0, 3.0));
    return v * mix(vec3<f32>(1.0, 1.0, 1.0), clamp(p - vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(0.0), vec3<f32>(1.0)), s);
}

const PI: f32 = 3.14159265;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let frag = in.clip.xy;             // framebuffer pixel coords, top-left origin
    let center = b.screen * 0.5;
    let p = frag - center;
    let hb = b.screen * 0.5;
    let sd = sd_round_box(p, hb, b.radius);

    // Keep only the ring: from the outer edge (sd=0) inward by `width`.
    if (sd > 0.0 || sd < -b.width) {
        discard;
    }

    // Antialias the two edges of the ring.
    let aa = 1.5;
    let outer = smoothstep(0.0, -aa, sd);
    let inner = smoothstep(-b.width, -b.width + aa, sd);
    let alpha = min(outer, inner);

    let angle = atan2(p.y, p.x) / (2.0 * PI) + 0.5; // 0..1 around the frame
    let hue = fract(angle * b.density + b.phase);
    let rgb = hsv2rgb(hue, 1.0, 1.0);
    return vec4<f32>(rgb * alpha, alpha); // premultiplied alpha
}
