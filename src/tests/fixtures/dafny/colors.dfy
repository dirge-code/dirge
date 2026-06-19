

module ConsoleColors {
        //https://www.shellhacks.com/bash-colors/
        const RL_START_IGNORE := 1 as char
        const RL_END_IGNORE := 2 as char
        const escape: char := 27 as char
        
        // Helper function to create an ANSI escape sequence
        function makeColor(code: string): string {
            [escape] + code
        }
        
        // Helper function to combine foreground and background codes
        // bgCode format: "[40m", fgCode format: "[1;31m" or "[31m"
        // Result: "[40;1;31m" or "[40;31m"
        function combineColors(fgCode: string, bgCode: string): string
            requires |bgCode| >= 3 && bgCode[0] == '[' && bgCode[|bgCode|-1] == 'm'
            requires |fgCode| >= 3 && fgCode[0] == '[' && fgCode[|fgCode|-1] == 'm'
        {
            // Extract numeric part from bgCode: "[40m" -> "40"
            var bgNum := bgCode[1..|bgCode|-1];
            // Extract numeric part from fgCode: "[1;31m" -> "1;31" or "[31m" -> "31"
            var fgNum := fgCode[1..|fgCode|-1];
            // Combine: "[40;1;31m"
            var combined := "[" + bgNum + ";" + fgNum + "m";
            makeColor(combined)
        }
        
        // Raw ANSI codes (without escape wrapper) for combining
        const FG_BLACK_CODE := "[1;30m"
        const FG_RED_CODE := "[1;31m"
        const FG_GREEN_CODE := "[1;32m"
        const FG_YELLOW_CODE := "[1;33m"
        const FG_BLUE_CODE := "[1;34m"
        const FG_MAGENTA_CODE := "[1;35m"
        const FG_CYAN_CODE := "[1;36m"
        const FG_WHITE_CODE := "[1;37m"
        
        const FG_BLACK_NORMAL_CODE := "[30m"
        const FG_RED_NORMAL_CODE := "[31m"
        const FG_GREEN_NORMAL_CODE := "[32m"
        const FG_YELLOW_NORMAL_CODE := "[33m"
        const FG_BLUE_NORMAL_CODE := "[34m"
        const FG_MAGENTA_NORMAL_CODE := "[35m"
        const FG_CYAN_NORMAL_CODE := "[36m"
        const FG_WHITE_NORMAL_CODE := "[37m"
        
        const BG_BLACK_CODE := "[40m"
        const BG_RED_CODE := "[41m"
        const BG_GREEN_CODE := "[42m"
        const BG_YELLOW_CODE := "[43m"
        const BG_BLUE_CODE := "[44m"
        const BG_MAGENTA_CODE := "[45m"
        const BG_CYAN_CODE := "[46m"
        const BG_WHITE_CODE := "[47m"
        
        // Foreground colors (bright/bold)
        const BLACK := makeColor(FG_BLACK_CODE)
        const RED := makeColor(FG_RED_CODE)
        const GREEN := makeColor(FG_GREEN_CODE)
        const YELLOW := makeColor(FG_YELLOW_CODE)
        const BLUE := makeColor(FG_BLUE_CODE)
        const MAGENTA := makeColor(FG_MAGENTA_CODE)
        const CYAN := makeColor(FG_CYAN_CODE)
        const WHITE := makeColor(FG_WHITE_CODE)
        
        // Foreground colors (normal)
        const BLACK_NORMAL := makeColor(FG_BLACK_NORMAL_CODE)
        const RED_NORMAL := makeColor(FG_RED_NORMAL_CODE)
        const GREEN_NORMAL := makeColor(FG_GREEN_NORMAL_CODE)
        const YELLOW_NORMAL := makeColor(FG_YELLOW_NORMAL_CODE)
        const BLUE_NORMAL := makeColor(FG_BLUE_NORMAL_CODE)
        const MAGENTA_NORMAL := makeColor(FG_MAGENTA_NORMAL_CODE)
        const CYAN_NORMAL := makeColor(FG_CYAN_NORMAL_CODE)
        const WHITE_NORMAL := makeColor(FG_WHITE_NORMAL_CODE)
        
        // Background colors
        const BG_BLACK := makeColor(BG_BLACK_CODE)
        const BG_RED := makeColor(BG_RED_CODE)
        const BG_GREEN := makeColor(BG_GREEN_CODE)
        const BG_YELLOW := makeColor(BG_YELLOW_CODE)
        const BG_BLUE := makeColor(BG_BLUE_CODE)
        const BG_MAGENTA := makeColor(BG_MAGENTA_CODE)
        const BG_CYAN := makeColor(BG_CYAN_CODE)
        const BG_WHITE := makeColor(BG_WHITE_CODE)
        
        // Reset/No color
        const NOCOLOR := makeColor("[0m")
        
        // Combined color function for common combinations
        function FgOnBg(fgCode: string, bgCode: string): string
            requires |bgCode| >= 3 && bgCode[0] == '[' && bgCode[|bgCode|-1] == 'm'
            requires |fgCode| >= 3 && fgCode[0] == '[' && fgCode[|fgCode|-1] == 'm'
        {
            combineColors(fgCode, bgCode)
        }

        // @Test
        method TestColors() {
            print "=== Foreground Colors (Bright/Bold) ===\n";
            print BLACK, "BLACK", NOCOLOR, "  ";
            print RED, "RED", NOCOLOR, "  ";
            print GREEN, "GREEN", NOCOLOR, "  ";
            print YELLOW, "YELLOW", NOCOLOR, "  ";
            print BLUE, "BLUE", NOCOLOR, "  ";
            print MAGENTA, "MAGENTA", NOCOLOR, "  ";
            print CYAN, "CYAN", NOCOLOR, "  ";
            print WHITE, "WHITE", NOCOLOR, "\n";
            
            print "\n=== Foreground Colors (Normal) ===\n";
            print BLACK_NORMAL, "BLACK_NORMAL", NOCOLOR, "  ";
            print RED_NORMAL, "RED_NORMAL", NOCOLOR, "  ";
            print GREEN_NORMAL, "GREEN_NORMAL", NOCOLOR, "  ";
            print YELLOW_NORMAL, "YELLOW_NORMAL", NOCOLOR, "  ";
            print BLUE_NORMAL, "BLUE_NORMAL", NOCOLOR, "  ";
            print MAGENTA_NORMAL, "MAGENTA_NORMAL", NOCOLOR, "  ";
            print CYAN_NORMAL, "CYAN_NORMAL", NOCOLOR, "  ";
            print WHITE_NORMAL, "WHITE_NORMAL", NOCOLOR, "\n";
            
            print "\n=== Background Colors ===\n";
            print BG_BLACK, "BG_BLACK", NOCOLOR, "  ";
            print BG_RED, "BG_RED", NOCOLOR, "  ";
            print BG_GREEN, "BG_GREEN", NOCOLOR, "  ";
            print BG_YELLOW, "BG_YELLOW", NOCOLOR, "  ";
            print BG_BLUE, "BG_BLUE", NOCOLOR, "  ";
            print BG_MAGENTA, "BG_MAGENTA", NOCOLOR, "  ";
            print BG_CYAN, "BG_CYAN", NOCOLOR, "  ";
            print BG_WHITE, "BG_WHITE", NOCOLOR, "\n";
            
            print "\n=== Color Grid (Foreground) ===\n";
            var colors := [BLACK, RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, WHITE];
            var names := ["BLACK", "RED", "GREEN", "YELLOW", "BLUE", "MAGENTA", "CYAN", "WHITE"];
            var i := 0;
            while i < |colors| {
                print colors[i], "ABC", NOCOLOR, " ";
                i := i + 1;
            }
            print "\n";
            i := 0;
            while i < |names| {
                print names[i], "  ";
                i := i + 1;
            }
            print "\n";
            
            print "\n=== Color Grid (Background) ===\n";
            var bgColors := [BG_BLACK, BG_RED, BG_GREEN, BG_YELLOW, BG_BLUE, BG_MAGENTA, BG_CYAN, BG_WHITE];
            var bgNames := ["BG_BLACK", "BG_RED", "BG_GREEN", "BG_YELLOW", "BG_BLUE", "BG_MAGENTA", "BG_CYAN", "BG_WHITE"];
            i := 0;
            while i < |bgColors| {
                print bgColors[i], "   ", NOCOLOR, " ";
                i := i + 1;
            }
            print "\n";
            i := 0;
            while i < |bgNames| {
                print bgNames[i], "  ";
                i := i + 1;
            }
            print "\n";
            
            print "\n=== Colored Text on Colored Backgrounds ===\n";
            print "Red text on white background: ";
            print FgOnBg(FG_RED_CODE, BG_WHITE_CODE), "Hello", NOCOLOR, "\n";
            
            print "Green text on black background: ";
            print FgOnBg(FG_GREEN_CODE, BG_BLACK_CODE), "Hello", NOCOLOR, "\n";
            
            print "Yellow text on blue background: ";
            print FgOnBg(FG_YELLOW_CODE, BG_BLUE_CODE), "Hello", NOCOLOR, "\n";
            
            print "Cyan text on red background: ";
            print FgOnBg(FG_CYAN_CODE, BG_RED_CODE), "Hello", NOCOLOR, "\n";
            
            print "White text on magenta background: ";
            print FgOnBg(FG_WHITE_CODE, BG_MAGENTA_CODE), "Hello", NOCOLOR, "\n";
            
            print "Blue text on yellow background: ";
            print FgOnBg(FG_BLUE_CODE, BG_YELLOW_CODE), "Hello", NOCOLOR, "\n";
            
            print "\n=== Color Combinations Grid ===\n";
            var fgCodes := [FG_RED_CODE, FG_GREEN_CODE, FG_YELLOW_CODE, FG_BLUE_CODE, FG_MAGENTA_CODE, FG_CYAN_CODE, FG_WHITE_CODE];
            var fgNames := ["RED", "GREEN", "YELLOW", "BLUE", "MAGENTA", "CYAN", "WHITE"];
            var bgCodes := [BG_BLACK_CODE, BG_RED_CODE, BG_GREEN_CODE, BG_BLUE_CODE, BG_MAGENTA_CODE, BG_CYAN_CODE, BG_WHITE_CODE];
            var bgNames2 := ["BG_BLACK", "BG_RED", "BG_GREEN", "BG_BLUE", "BG_MAGENTA", "BG_CYAN", "BG_WHITE"];
            
            var fgIdx := 0;
            while fgIdx < |fgCodes| {
                var bgIdx := 0;
                while bgIdx < |bgCodes| {
                    print FgOnBg(fgCodes[fgIdx], bgCodes[bgIdx]), "X", NOCOLOR, " ";
                    bgIdx := bgIdx + 1;
                }
                print " ", fgNames[fgIdx], "\n";
                fgIdx := fgIdx + 1;
            }
            print " ";
            var bgIdx2 := 0;
            while bgIdx2 < |bgNames2| {
                print bgNames2[bgIdx2], " ";
                bgIdx2 := bgIdx2 + 1;
            }
            print "\n";
            print "\n", RED, "RED", NOCOLOR, "\n";
        }

}