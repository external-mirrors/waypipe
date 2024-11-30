#version 450

layout(binding = 0, rgba8) uniform writeonly image2D output_img;
layout(binding = 1) uniform textureBuffer input_y;
layout(binding = 2) uniform textureBuffer input_u;
layout(binding = 3) uniform textureBuffer input_v;

layout(push_constant) uniform constants {
  mat3x4 ybr_to_rgb;
  int stride_y;
  int stride_u;
  int stride_v;
}
push;

layout(local_size_x = 8, local_size_y = 8, local_size_z = 1) in;
void main() {
  ivec2 outdim = imageSize(output_img);

  /* invocation x/y ranges over all pixels of output */
  ivec2 pos = ivec2(gl_GlobalInvocationID.xy);

  // TODO: any better strategy than doubling U/V coordinates?
  int y_pos = pos.y * push.stride_y + pos.x;
  int u_pos = (pos.y / 2) * push.stride_u + (pos.x / 2);
  int v_pos = (pos.y / 2) * push.stride_v + (pos.x / 2);

  float y = texelFetch(input_y, y_pos).r;
  float b = texelFetch(input_v, v_pos).r;
  float r = texelFetch(input_u, u_pos).r;
  vec4 ybro = vec4(y, b, r, 1.0);
  vec3 rgb = transpose(push.ybr_to_rgb) * ybro;

  vec4 val = vec4(rgb, 1);
  ivec2 opos = ivec2(gl_GlobalInvocationID.xy);
  if (opos.x < outdim.x && opos.y < outdim.y) {
    imageStore(output_img, opos, val);
  }
}
