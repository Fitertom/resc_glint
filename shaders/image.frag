#version 450
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(push_constant) uniform P { vec4 rect; vec4 m; vec4 o; vec4 bg; } p;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 outc;

void main() {
    if (p.o.w > 2.5) {
        // Frosted glass: the photo under the toolbar, blurred. o.w - 3 is the corner radius,
        // bg the bar's rectangle in pixels. The blur comes from the mip chain: a level about
        // half the radius wide, then a spiral of taps over it, so a wide blur costs 16 reads.
        vec2 size = vec2(textureSize(tex, 0));
        float r = 22.0 / max(p.o.z, 1e-4); // radius in texels; o.z = screen pixels per texel
        float lod = max(log2(r * 0.35), 0.0);
        vec4 acc = vec4(0.0);
        for (int i = 0; i < 16; i++) {
            float k = (float(i) + 0.5) / 16.0;
            float a = float(i) * 2.39996; // golden angle
            acc += textureLod(tex, uv + vec2(cos(a), sin(a)) * sqrt(k) * r / size, lod);
        }
        acc /= 16.0;
        float rad = p.o.w - 3.0;
        vec2 q = abs(gl_FragCoord.xy - (p.bg.xy + p.bg.zw) * 0.5) - (p.bg.zw - p.bg.xy) * 0.5 + rad;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - rad;
        outc = acc * clamp(0.5 - d, 0.0, 1.0); // premultiplied, blended by the pipeline
        return;
    }
    if (p.o.w > 1.5) { // a flat premultiplied colour: the shade behind the setup card
        outc = p.bg;
        return;
    }
    float scale = p.o.z; // screen pixels per texel
    vec4 c;
    if (scale > 1.0) {
        // Sharp bilinear: plain bilinear near 1:1, crisp pixel squares with a one-pixel soft
        // edge when zoomed in. Nearest would shimmer at 1.5x, bilinear turns 8x into mush.
        vec2 size = vec2(textureSize(tex, 0));
        vec2 t = uv * size;
        vec2 fl = floor(t);
        vec2 d = fract(t) - 0.5;
        float region = 0.5 - 0.5 / scale;
        vec2 f = (d - clamp(d, -region, region)) * scale + 0.5;
        c = textureLod(tex, (fl + f) / size, 0.0);
    } else {
        c = texture(tex, uv); // trilinear over the mip chain
    }
    if (p.o.w > 0.5) { // overlay: premultiplied, blended by the pipeline
        outc = c;
        return;
    }
    vec3 bg = p.bg.rgb;
    if (p.bg.w > 0.5) {
        vec2 q = floor(gl_FragCoord.xy / 12.0);
        bg = mod(q.x + q.y, 2.0) < 1.0 ? vec3(0.42) : vec3(0.58);
    }
    outc = vec4(c.rgb + bg * (1.0 - c.a), 1.0); // texture is premultiplied BGRA
}
