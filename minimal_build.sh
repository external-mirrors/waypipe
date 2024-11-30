#!/bin/sh
set -e

echo "This script is a backup build system in case meson/ninja are unavailable."
echo "No optional features or optimizations are included. Waypipe will be slow."
echo "Requirements: python3, gcc, libc+pthreads"
echo "Enter to continue, interrupt to exit."
read unused

mkdir -p build-minimal
cd build-minimal
root=../waypipe-c

echo "Generating code..."
python3 $root/protocols/symgen.py data $root/protocols/function_list.txt protocols.c \
    ../protocols/*.xml
python3 $root/protocols/symgen.py header $root/protocols/function_list.txt protocols.h \
    ../protocols/*.xml
echo '#define WAYPIPE_VERSION "minimal"' > config-waypipe.h

echo "Compiling..."
gcc -D_DEFAULT_SOURCE -Os -I. -I$root/protocols/ -lpthread -o waypipe-c protocols.c \
    $root/bench.c $root/client.c $root/dmabuf.c $root/handlers.c \
    $root/interval.c $root/kernel.c $root/mainloop.c $root/parsing.c \
    $root/platform.c $root/server.c $root/shadow.c $root/util.c \
    $root/video.c $root/waypipe.c

cd ..
echo "Done. See ./build-minimal/waypipe-c"
