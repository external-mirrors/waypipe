#!/bin/sh
set -e
black -q waypipe-c/test/*.py waypipe-c/protocols/*.py
clang-format -style=file --assume-filename=C -i waypipe-c/*.h waypipe-c/*.c waypipe-c/test/*.c waypipe-c/test/*.h
