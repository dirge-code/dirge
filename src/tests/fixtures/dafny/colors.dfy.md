# colors.dfy - ANSI Terminal Colors

A module providing ANSI escape codes for terminal text coloring in Dafny programs. Supports foreground colors, background colors, and combinations of both.

## Overview

This module provides constants and functions for adding color to terminal output using ANSI escape sequences. It supports both bright/bold and normal intensity foreground colors, background colors, and the ability to combine foreground and background colors in a single escape sequence.

## Constants

### Foreground Colors (Bright/Bold)

These constants provide bright, bold text colors:

- **`BLACK`** - Bright black text
- **`RED`** - Bright red text
- **`GREEN`** - Bright green text
- **`YELLOW`** - Bright yellow text
- **`BLUE`** - Bright blue text
- **`MAGENTA`** - Bright magenta text
- **`CYAN`** - Bright cyan text
- **`WHITE`** - Bright white text

### Foreground Colors (Normal)

These constants provide normal intensity text colors:

- **`BLACK_NORMAL`** - Normal black text
- **`RED_NORMAL`** - Normal red text
- **`GREEN_NORMAL`** - Normal green text
- **`YELLOW_NORMAL`** - Normal yellow text
- **`BLUE_NORMAL`** - Normal blue text
- **`MAGENTA_NORMAL`** - Normal magenta text
- **`CYAN_NORMAL`** - Normal cyan text
- **`WHITE_NORMAL`** - Normal white text

### Background Colors

These constants provide background colors:

- **`BG_BLACK`** - Black background
- **`BG_RED`** - Red background
- **`BG_GREEN`** - Green background
- **`BG_YELLOW`** - Yellow background
- **`BG_BLUE`** - Blue background
- **`BG_MAGENTA`** - Magenta background
- **`BG_CYAN`** - Cyan background
- **`BG_WHITE`** - White background

### Reset

- **`NOCOLOR`** - Resets all color formatting to default

## Functions

### Function: `FgOnBg(fgCode: string, bgCode: string): string`

Combines a foreground color code with a background color code into a single ANSI escape sequence.

**Parameters:**
- `fgCode`: A foreground color code constant (e.g., `FG_RED_CODE`, `FG_GREEN_CODE`)
- `bgCode`: A background color code constant (e.g., `BG_WHITE_CODE`, `BG_BLACK_CODE`)

**Returns:**
- A combined ANSI escape sequence string that sets both foreground and background colors

**Preconditions:**
- Both codes must be valid ANSI code strings in the format `"[XXm"` or `"[X;XXm"`

**Example:**
```dafny
import opened ConsoleColors

var combo := FgOnBg(FG_RED_CODE, BG_WHITE_CODE);
print combo, "Red on white", NOCOLOR, "\n";
```

## Raw Code Constants

For use with `FgOnBg`, the module provides raw ANSI code constants:

**Foreground codes (bright):**
- `FG_BLACK_CODE`, `FG_RED_CODE`, `FG_GREEN_CODE`, `FG_YELLOW_CODE`
- `FG_BLUE_CODE`, `FG_MAGENTA_CODE`, `FG_CYAN_CODE`, `FG_WHITE_CODE`

**Foreground codes (normal):**
- `FG_BLACK_NORMAL_CODE`, `FG_RED_NORMAL_CODE`, `FG_GREEN_NORMAL_CODE`, etc.

**Background codes:**
- `BG_BLACK_CODE`, `BG_RED_CODE`, `BG_GREEN_CODE`, `BG_YELLOW_CODE`
- `BG_BLUE_CODE`, `BG_MAGENTA_CODE`, `BG_CYAN_CODE`, `BG_WHITE_CODE`

## Usage Examples

### Basic Foreground Colors

```dafny
import opened ConsoleColors

print RED, "Error message", NOCOLOR, "\n";
print GREEN, "Success message", NOCOLOR, "\n";
print YELLOW, "Warning message", NOCOLOR, "\n";
```

### Background Colors

```dafny
import opened ConsoleColors

print BG_RED, "Important", NOCOLOR, "\n";
print BG_GREEN, "Safe", NOCOLOR, "\n";
```

### Combined Foreground and Background

```dafny
import ConsoleColors

// Red text on white background
print FgOnBg(FG_RED_CODE, BG_WHITE_CODE), "Alert", NOCOLOR, "\n";

// Green text on black background
print FgOnBg(FG_GREEN_CODE, BG_BLACK_CODE), "Success", NOCOLOR, "\n";

// Yellow text on blue background
print FgOnBg(FG_YELLOW_CODE, BG_BLUE_CODE), "Info", NOCOLOR, "\n";
```

### Color-Coded Output

```dafny
import opened ConsoleColors

method PrintStatus(status: string, isError: bool) {
    if isError {
        print RED, "ERROR: ", NOCOLOR, status, "\n";
    } else {
        print GREEN, "OK: ", NOCOLOR, status, "\n";
    }
}
```

### Highlighting Specific Text

```dafny
import opened ConsoleColors

print "The value is: ";
print CYAN, value, NOCOLOR;
print "\n";
```

## Important Notes

1. **Always reset colors**: Always use `NOCOLOR` after colored text to reset formatting. Otherwise, the color will continue to affect subsequent output.

2. **Terminal support**: ANSI color codes work in most modern terminals (bash, zsh, PowerShell, Windows Terminal, etc.). Some older terminals may not support colors.

3. **Combining colors**: When combining foreground and background colors, use the `FgOnBg` function with the raw code constants (`FG_*_CODE` and `BG_*_CODE`). This ensures the colors are combined in a single ANSI escape sequence.

4. **Print statement format**: The typical pattern is:
   ```dafny
   print COLOR, "text", NOCOLOR, "more text\n";
   ```

5. **Testing**: The module includes a `TestColors()` method (marked with `{:test}`) that demonstrates all available colors and combinations. Run it with the dafny test command to see how colors appear in your terminal. `dafny test --allow-warnings .\utils\colors.dfy`

## Technical Details

- Colors use standard ANSI escape sequences (ESC[XXm format)
- The module uses escape character 27 (0x1B) for ANSI sequences
- Bright colors use the `1;` prefix (e.g., `[1;31m` for bright red)
- Normal colors use direct codes (e.g., `[31m` for normal red)
- Background colors use codes 40-47
- Foreground colors use codes 30-37 (normal) or 1;30-1;37 (bright)

## Reference

Based on ANSI color codes as documented at: https://www.shellhacks.com/bash-colors/

