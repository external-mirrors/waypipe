#version 450

layout(binding = 0) uniform sampler2D input_rgb;
layout(binding = 1, r8) writeonly uniform imageBuffer output_y;
layout(binding = 2, r8) writeonly uniform imageBuffer output_u;
layout(binding = 3, r8) writeonly uniform imageBuffer output_v;

layout(push_constant) uniform constants {
  mat3x4 rgb_to_yrb;
  int stride_y;
  int stride_u;
  int stride_v;
}
push;

/* Each individual task fills 2x2 pixels. */
/* note: AMD subgroupSize is 64, so 8x8 is needed to fully use it */
layout(local_size_x = 8, local_size_y = 8, local_size_z = 1) in;
void main() {
  ivec2 rgbdim = textureSize(input_rgb, 0);

  vec4 avg = vec4(0.);
  for (int i = 0; i < 2; i++) {
    for (int j = 0; j < 2; j++) {
      ivec2 pos = ivec2(2 * gl_GlobalInvocationID.x + i,
                        2 * gl_GlobalInvocationID.y + j);
      vec2 sample_pos = vec2(pos) + 0.5;
      vec4 rgbo = vec4(texture(input_rgb, sample_pos).rgb, 1.0);
      float y = (transpose(push.rgb_to_yrb) * rgbo).r;
      int store_pos = pos.y * push.stride_y + pos.x;
      imageStore(output_y, store_pos, vec4(y, 1., 1., 1.));
      avg += rgbo;
    }
  }
  ivec2 pos = ivec2(gl_GlobalInvocationID.xy);
  vec2 sample_pos = vec2(2 * pos + 0.5) + 0.5;
  //   vec4 rgbo = vec4(texture(input_rgb, sample_pos).rgb, 1.0);
  vec4 rgbo = avg / 4;
  vec2 vu = (transpose(push.rgb_to_yrb) * rgbo).bg;
  int store_pos_v = pos.y * push.stride_v + pos.x;
  imageStore(output_v, store_pos_v, vec4(vu.g, 1., 1., 1.));
  int store_pos_u = pos.y * push.stride_u + pos.x;
  imageStore(output_u, store_pos_u, vec4(vu.r, 1., 1., 1.));
}
