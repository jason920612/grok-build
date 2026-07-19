#!/bin/bash
/home/jason/grok-build-target/debug/ptyctl screen -n rhythm 2>&1 | tail -n "${1:-22}"
