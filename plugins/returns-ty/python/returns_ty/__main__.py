"""Run ty-extended with returns support, or print its configuration."""

from __future__ import annotations

import argparse
import subprocess
import sys

from ty_extended import find_ty_extended_bin

from .runtime import checker_options, config_overrides


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--print-config", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.print_config:
        if args.command:
            parser.error("--print-config does not take a checker command")
        print("\n".join(config_overrides()))
        return
    if not args.command or args.command[0] != "check":
        parser.error("expected check followed by ty-extended options and paths")
    executable = str(find_ty_extended_bin())
    sys.exit(
        subprocess.call([executable, "check", *checker_options(), *args.command[1:]])
    )


if __name__ == "__main__":
    main()
