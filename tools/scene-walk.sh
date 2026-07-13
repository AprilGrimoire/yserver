#!/bin/sh
# Log the scene-walk trace for this smoke run.
# Enable the scene probe globally so the current session's XIDs
# do not need to be known in advance.
YSERVER_SCENE_WALK_ALL=1 just yserver-fvwm3-xterm-hw
