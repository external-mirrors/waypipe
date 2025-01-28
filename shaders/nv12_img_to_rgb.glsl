#version 450

layout(binding = 0, rgba8) uniform writeonly image2D output_img;
layout(binding = 1) uniform sampler2D input_y;
layout(binding = 2) uniform sampler2D input_vu;

layout(push_constant) uniform constants { mat3x4 ybr_to_rgb; }
push;

layout(local_size_x = 8, local_size_y = 8, local_size_z = 1) in;
void main() {
  ivec2 brdim = textureSize(input_vu, 0);
  ivec2 ydim = textureSize(input_y, 0);
  ivec2 outdim = imageSize(output_img);
  /* input_vu has exactly half the size of input_y */

  /* invocation x/y ranges over all pixels of output */
  vec2 pos = min(vec2(gl_GlobalInvocationID.xy) + 0.5, vec2(ydim));

  float y = texture(input_y, pos).r;
  vec2 br = texture(input_vu, pos / 2.0).gr;
  vec4 ybro = vec4(y, br.r, br.g, 1.0);
  vec3 rgb = transpose(push.ybr_to_rgb) * ybro;

  vec4 val = vec4(rgb, 1);
  ivec2 opos = ivec2(gl_GlobalInvocationID.xy);
  if (opos.x < outdim.x && opos.y < outdim.y) {
    imageStore(output_img, opos, val);
  }
}
