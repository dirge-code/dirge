"""
Minimal Python module for DAP module-launch integration test.

Run with: python -m test_mod
"""

from __future__ import annotations


def main():
    x = 1
    y = 2
    z = x + y
    breakpoint()  # stop here for DAP
    print(f"z = {z}")


if __name__ == "__main__":
    main()
