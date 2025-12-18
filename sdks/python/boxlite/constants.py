"""
Centralized constants for BoxLite Python SDK.
"""

# Default VM resources
DEFAULT_CPUS = 1
DEFAULT_MEMORY_MIB = 2048

# ComputerBox defaults (higher resources for desktop)
COMPUTERBOX_CPUS = 2
COMPUTERBOX_MEMORY_MIB = 2048
COMPUTERBOX_IMAGE = "lscr.io/linuxserver/webtop:ubuntu-xfce"

# ComputerBox display settings
COMPUTERBOX_DISPLAY_NUMBER = ":1"
COMPUTERBOX_DISPLAY_WIDTH = 1024
COMPUTERBOX_DISPLAY_HEIGHT = 768

# ComputerBox network ports (webtop defaults)
COMPUTERBOX_GUI_HTTP_PORT = 3000
COMPUTERBOX_GUI_HTTPS_PORT = 3001

# Timeouts (seconds)
DESKTOP_READY_TIMEOUT = 60
DESKTOP_READY_RETRY_DELAY = 0.5
