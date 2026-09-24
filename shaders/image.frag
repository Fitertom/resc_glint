#version 450
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(push_constant) uniform P { vec4 rect; vec4 m; vec4 o; vec4 bg; } p;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 outc;

void main() {
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
