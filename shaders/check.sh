#!/bin/sh
glslc -Werror -O -fshader-stage=compute nv12_img_to_rgb.glsl   -o - >/dev/null
glslc -Werror -O -fshader-stage=compute rgb_to_nv12_img.glsl   -o - >/dev/null
glslc -Werror -O -fshader-stage=compute rgb_to_yuv420_buf.glsl -o - >/dev/null
glslc -Werror -O -fshader-stage=compute yuv420_buf_to_rgb.glsl -o - >/dev/null
