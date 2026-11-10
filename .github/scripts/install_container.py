"""The non-root identity both Linux installer containers give Blaze's sandbox launcher.

Stage 0 e930296025's confine.rs starts the Linux launcher with an empty environment.
Its sandbox/linux.rs then finds the interposer cache through std::env::home_dir(), whose
Unix implementation calls getpwuid_r. Docker's numeric --user alone supplies no such entry.
The account belongs to the image; its writable cache belongs to the verified job volume.
"""

from dataclasses import dataclass
import json
from pathlib import Path


@dataclass(frozen=True)
class User:
    uid: int
    gid: int
    home: str

    def __post_init__(self) -> None:
        if any(type(value) is not int or not 0 < value < 2**32 - 1 for value in (self.uid, self.gid)):
            raise ValueError("installer uid and gid must be non-root Linux user and group ids")
        path = Path(self.home)
        if not path.is_absolute() or ".." in path.parts or any(char in self.home for char in ":\n\r\0"):
            raise ValueError(f"installer home must be absolute with no parent traversal or passwd field delimiters: {self.home!r}")

    @property
    def ids(self) -> str:
        return f"{self.uid}:{self.gid}"

    def dockerfile(self, *, create_home: bool) -> str:
        """Register the exact runtime uid/gid/home without importing host account files."""
        commands = [
            ["groupadd", "--gid", str(self.gid), "blaze"],
            ["useradd", "--uid", str(self.uid), "--gid", str(self.gid),
             "--home-dir", self.home, "--create-home" if create_home else "--no-create-home",
             "--no-log-init", "--shell", "/bin/sh", "blaze"],
        ]
        # Exec-form RUN keeps spaces, quotes and shell metacharacters in the home literal.
        return "".join("RUN " + json.dumps(command) + "\n" for command in commands)


PREFLIGHT = r'''
set -eu
uid=$(id -u)
gid=$(id -g)
[ "$uid" -ne 0 ] && [ "$gid" -ne 0 ] || { echo "::error::installer container must run as a non-root user and group"; exit 1; }
account=$(env -i "$(command -v getent)" passwd "$uid") || { echo "::error::installer uid $uid has no passwd entry with an empty environment"; exit 1; }
identity=$(printf '%s\n' "$account" | cut -d: -f3,4,6)
[ "$identity" = "$uid:$gid:$HOME" ] || { echo "::error::installer passwd identity $identity does not match $uid:$gid:$HOME"; exit 1; }
probe=$(mktemp "$HOME/.cache/blaze-install.XXXXXXXX") || { echo "::error::installer uid $uid cannot write its passwd home cache $HOME/.cache"; exit 1; }
rm -- "$probe"
'''
