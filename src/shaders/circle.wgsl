// Filled, anti-aliased circles in pixel space (macOS-style window controls).

struct Globals { screen: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> g: Globals;

struct VsIn {
    @location(0) center: vec2<f32>,
    @location(1) radius: f32,
    @location(2) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, in: VsIn) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let px = in.center + c * in.radius;
    let ndc = vec2<f32>(px.x / g.screen.x * 2.0 - 1.0, 1.0 - px.y / g.screen.y * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = c * in.radius;
    out.color = in.color;
    out.radius = in.radius;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = length(in.local);
    let a = 1.0 - smoothstep(in.radius - 1.0, in.radius, d);
    let pa = in.color.a * a;
    return vec4<f32>(in.color.rgb * pa, pa); // premultiplied alpha
}
