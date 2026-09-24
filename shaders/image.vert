#version 450
// One quad, four vertices of a strip, no vertex buffer: the corners come from the index.
layout(push_constant) uniform P { vec4 rect; vec4 m; vec4 o; vec4 bg; } p;
layout(location = 0) out vec2 uv;
void main() {
    vec2 c = vec2(gl_VertexIndex & 1, gl_VertexIndex >> 1);
    gl_Position = vec4(mix(p.rect.xy, p.rect.zw, c), 0.0, 1.0);
    // Orientation (EXIF + user rotation) is a 2x2 integer matrix on the corner, not a
    // rotated copy of the pixels.
    uv = mat2(p.m.xy, p.m.zw) * c + p.o.xy;
}
