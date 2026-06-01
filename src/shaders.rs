pub const GL33_VERTEX_SHADER: &str = r#"#version 330 core
layout (location = 0) in vec2 position;
layout (location = 1) in vec2 tex_coord;

uniform vec2 viewport;
out vec2 uv;

void main() {
    vec2 ndc = vec2(
        (position.x / viewport.x) * 2.0 - 1.0,
        1.0 - (position.y / viewport.y) * 2.0
    );
    gl_Position = vec4(ndc, 0.0, 1.0);
    uv = tex_coord;
}
"#;

pub const GL33_FRAGMENT_SHADER: &str = r#"#version 330 core
in vec2 uv;

uniform sampler2D image_texture;
uniform vec2 spotlight_center;
uniform float spotlight_radius;
uniform float spotlight_tint;
uniform int spotlight_enabled;
uniform float fade_alpha;
out vec4 color;

void main() {
    vec4 pixel = texture(image_texture, uv);

    if (spotlight_enabled == 1) {
        float distance_from_center = distance(gl_FragCoord.xy, spotlight_center);
        if (distance_from_center > spotlight_radius) {
            pixel.rgb *= spotlight_tint;
        }
    }

    color = vec4(pixel.rgb * fade_alpha, pixel.a * fade_alpha);
}
"#;

pub const GL20_VERTEX_SHADER: &str = r#"#version 110
attribute vec2 position;
attribute vec2 tex_coord;

uniform vec2 viewport;
varying vec2 uv;

void main() {
    vec2 ndc = vec2(
        (position.x / viewport.x) * 2.0 - 1.0,
        1.0 - (position.y / viewport.y) * 2.0
    );
    gl_Position = vec4(ndc, 0.0, 1.0);
    uv = tex_coord;
}
"#;

pub const GL20_FRAGMENT_SHADER: &str = r#"#version 110
varying vec2 uv;

uniform sampler2D image_texture;
uniform vec2 spotlight_center;
uniform float spotlight_radius;
uniform float spotlight_tint;
uniform int spotlight_enabled;
uniform float fade_alpha;

void main() {
    vec4 pixel = texture2D(image_texture, uv);

    if (spotlight_enabled == 1) {
        float distance_from_center = distance(gl_FragCoord.xy, spotlight_center);
        if (distance_from_center > spotlight_radius) {
            pixel.rgb *= spotlight_tint;
        }
    }

    gl_FragColor = vec4(pixel.rgb * fade_alpha, pixel.a * fade_alpha);
}
"#;

pub const GLES2_VERTEX_SHADER: &str = r#"#version 100
attribute vec2 position;
attribute vec2 tex_coord;

uniform vec2 viewport;
varying vec2 uv;

void main() {
    vec2 ndc = vec2(
        (position.x / viewport.x) * 2.0 - 1.0,
        1.0 - (position.y / viewport.y) * 2.0
    );
    gl_Position = vec4(ndc, 0.0, 1.0);
    uv = tex_coord;
}
"#;

pub const GLES2_FRAGMENT_SHADER: &str = r#"#version 100
precision mediump float;

varying vec2 uv;

uniform sampler2D image_texture;
uniform vec2 spotlight_center;
uniform float spotlight_radius;
uniform float spotlight_tint;
uniform int spotlight_enabled;
uniform float fade_alpha;

void main() {
    vec4 pixel = texture2D(image_texture, uv);

    if (spotlight_enabled == 1) {
        float distance_from_center = distance(gl_FragCoord.xy, spotlight_center);
        if (distance_from_center > spotlight_radius) {
            pixel.rgb *= spotlight_tint;
        }
    }

    gl_FragColor = vec4(pixel.rgb * fade_alpha, pixel.a * fade_alpha);
}
"#;
