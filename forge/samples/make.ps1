# Writes the hand-made samples that carry escape bytes:
#   draw.vt                  box drawing and underline styles, to be looked at (proteus shot --vt)
#   conformance.vt           80x24 exercise of the terminal, checked cell by cell by `cargo test conform`
#   conformance.expect.txt   what the screen must say afterwards, written by hand from the
#                            specifications (never copied from a terminal's output)
#   pwsh forge\samples\make.ps1
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$e = [char]27
$bel = [char]7
$utf8 = [Text.UTF8Encoding]::new($false)

# ------------------------------------------------------------------ draw.vt
$h = "$e[1;38;2;94;230;255m"
$z = "$e[0m"
$draw = @(
  "${h}double lines${z} $e[2m(U+2550 to U+256C, drawn by the engine)$z",
  "",
  "╔═══════╦═══════╗  ╒═══════╤═══════╕  ╓───────╥───────╖  ┌───────┬───────┐  ╔═════════════╗",
  "║ double║ all   ║  │ double│ across│  ║ double║ down  ║  │ single│ light │  ║ ┌─────────┐ ║",
  "╠═══════╬═══════╣  ╞═══════╪═══════╡  ╟───────╫───────╢  ├───────┼───────┤  ║ │ nested  │ ║",
  "║ round ║ about ║  │ single│ down  │  ║ single║ across║  │ for   │ scale │  ║ └─────────┘ ║",
  "╚═══════╩═══════╝  ╘═══════╧═══════╛  ╙───────╨───────╜  └───────┴───────┘  ╚═════════════╝",
  "",
  "all 29   ═ ║ ╒ ╓ ╔ ╕ ╖ ╗ ╘ ╙ ╚ ╛ ╜ ╝ ╞ ╟ ╠ ╡ ╢ ╣ ╤ ╥ ╦ ╧ ╨ ╩ ╪ ╫ ╬",
  "joined   ═══╪═══╤═══╧═══╬═══  ──╫──╥──╨──  ╔╗ ╒╕ ╓╖  ╔╦╗ ╒╤╕ ╓╥╖   diagonals ╱╲╳ ╱╱╱ ╲╲╲ ╳╳╳  ╱╲",
  "            │   │       ║       ║  ║       ╚╝ ╘╛ ╙╜  ╠╬╣ ╞╪╡ ╟╫╢               ╲╱    ╱ ╲     ╲╱",
  "                                                     ╚╩╝ ╘╧╛ ╙╨╜",
  "",
  "${h}underlines${z} $e[2m(SGR 4, 4:2, 4:3, 4:4, 4:5, 21 and SGR 58 colours)$z",
  "",
  "$e[4mplain underline$z   $e[4:2mdouble underline$z   $e[4:3mundercurl is a wave$z   $e[4:4mdotted underline$z   $e[4:5mdashed underline$z",
  "",
  "$e[4:3;58;2;255;122;144mspelling errror$z   $e[4:3;58;5;39mindexed blue curl$z   $e[4:4;58:2::255:207:92mdotted amber$z   $e[4:5;58;2;79;227;166mdashed green$z   $e[4;58;2;255;122;144mred line$z",
  "",
  "$e[4:3mgjpqy_gjpqy$z   $e[4:4mgjpqy_gjpqy$z   $e[4:5mgjpqy_gjpqy$z   $e[4mgjpqy_gjpqy$z   $e[21mgjpqy_gjpqy$z   $e[4:3m   across   gaps   $z",
  "",
  "$e[1;4:3mbold curl$z  $e[3;4:4mitalic dotted$z  $e[9;4:5mstruck dashed$z  $e[7;4:3m inverse curl $z  $e[2;4:3mdim curl$z  $e[4:3m日本語 한국어$z  $e[4:4m日本語$z  $e[4:5m日本語$z",
  "",
  "$e[38;2;255;122;144m$e[4:3merror[E0308]${z}: mismatched types   $e[38;2;255;207;92m$e[4:4mwarning${z}: unused variable   $e[38;2;79;227;166m$e[4:5mhint${z}: dashes for a suggestion"
)
[IO.File]::WriteAllText("$here\draw.vt", ($draw -join "`r`n") + "`r`n", $utf8)

# ----------------------------------------------------------- conformance.vt
# Everything is placed with absolute addresses on an 80x24 screen. Rows are
# 1-based here, as in the escape codes.
function At([int]$row, [int]$col) { "$e[$row;${col}H" }
$acute = [char]0x301; $diaeresis = [char]0x308; $dotbelow = [char]0x323
$zwj = [char]0x200D; $vs16 = [char]0xFE0F
$grin = [char]::ConvertFromUtf32(0x1F600)
$heart = "$([char]0x2764)$vs16"
$thumb = [char]::ConvertFromUtf32(0x1F44D) + [char]::ConvertFromUtf32(0x1F3FD)
$family = [char]::ConvertFromUtf32(0x1F468) + $zwj + [char]::ConvertFromUtf32(0x1F469) + $zwj + [char]::ConvertFromUtf32(0x1F467)
$flag = [char]::ConvertFromUtf32(0x1F1EF) + [char]::ConvertFromUtf32(0x1F1F5)

$c = [Text.StringBuilder]::new()
function Emit([string]$s) { [void]$c.Append($s) }

Emit "$e[2J$e[H$e]2;conformance$bel"
Emit "$(At 1 1)PROTEUS conformance sample (80x24): compare with conformance.expect.txt"

# Row 2: character attributes and their resets.
Emit "$(At 2 1)$e[1mbold$e[0m $e[2mdim$e[0m $e[3mitalic$e[0m $e[7minverse$e[0m $e[8mhidden$e[0m $e[9mstrike$e[0m "
Emit "$e[1;2;3;7;8;9m$e[22;23;27;28;29mplain$e[0m"

# Row 3: every underline style, and switching between them.
Emit "$(At 3 1)$e[4mU1$e[0m $e[4:2mU2$e[0m $e[4:3mU3$e[0m $e[4:4mU4$e[0m $e[4:5mU5$e[0m $e[21mD$e[0m "
Emit "$e[4mx$e[24my$e[4:3mz$e[4:0mw$e[4:1mv$e[0m"

# Row 4: every colour form, one letter each.
Emit "$(At 4 1)$e[31ma$e[91mb$e[38;5;208mc$e[38;2;1;2;3md$e[38:2::4:5:6me$e[38:2:7:8:9mf$e[38:5:39mg$e[39mh"
Emit "$e[41mi$e[101mj$e[48;5;17mk$e[48;2;10;20;30ml$e[48:2::40:50:60mm$e[49mn"
Emit "$e[4;58;2;11;12;13mo$e[58:2::14:15:16mp$e[58;5;99mq$e[58:5:98mr$e[59ms$e[0mt$e[1;31;42mu$e[22;39;49mv$e[0m"

# Rows 5 and 6: cursor movement.
Emit "$(At 5 1)0123456789$(At 5 3)A$e[2CB$e[3DC$e[8GD$e[1BE$e[1AF$e[EG$e[FH"
Emit "$(At 5 12)I$e[6;12fJ${e}7$(At 5 14)K${e}8L$e[s$(At 5 16)M$e[uN"

# Row 7: tab stops (default every 8, set, back-tab, clear one).
Emit "$(At 7 1)a`tb`tc$(At 7 21)${e}H$(At 7 18)`td`te$e[2ZF$(At 7 21)$e[g$(At 7 18)`tg"

# Row 8: wide characters, an emoji, combining marks.
Emit "$(At 8 1)日本語 한 $grin e$acute a$diaeresis$dotbelow ."

# Rows 9 and 10: a wide character that does not fit wraps whole; overwriting
# half of one removes all of it.
Emit "$(At 9 79)x日$(At 10 4)日本$(At 10 5)X$(At 10 10)語$(At 10 10)Y"

# Row 11: insert and delete characters, erase character, insert mode, erase to end of line.
Emit "$(At 11 1)abcdefghij$(At 11 3)$e[2@$(At 11 8)$e[3P$(At 11 2)$e[1X$e[4h$(At 11 5)XY$e[4l$(At 11 20)tail$(At 11 22)$e[K"

# Row 12: erase to start of line, then to end of line.
Emit "$(At 12 1)0123456789ABCDEFGHIJ$(At 12 6)$e[1K$(At 12 16)$e[0K"

# Row 18 is written before the region below it is exercised, and must not move.
Emit "$(At 18 1)guard: this row sits just below the scroll region and must not move"

# Rows 13 to 17: a scroll region. Line feed at the bottom, insert line,
# delete line, reverse index at the top, scroll up, scroll down, origin mode.
Emit "$e[13;17r$(At 13 1)r1`r`nr2`r`nr3`r`nr4`r`nr5`n`rr6"
Emit "$(At 14 1)$e[1Lil$(At 16 1)$e[1M$(At 13 1)${e}Mri$e[1S$e[2T"
Emit "$e[?6h$(At 1 1)om$(At 2 4)X$(At 9 1)Z$e[?6l$e[r"

# Row 19: the alternate screen keeps the main one intact and the cursor's place.
Emit "$(At 19 1)alt:$e[?1049h$(At 1 1)ALT SCREEN$(At 2 1)should vanish$e[?1049lback"

# Rows 20 and 21: wrapping at the right edge, and not wrapping when told not to.
Emit "$(At 20 76)wrapped$e[?7l$(At 21 78)NOWRAP$e[?7h"

# Row 22: repeat, the line-drawing character set, a hyperlink.
Emit "$(At 22 1)x$e[3b $e(0lqqk$e(B $e]8;;https://example.com/${bel}link$e]8;;$bel"

# Row 23: with every tab stop cleared a tab goes to the last column.
Emit "$(At 23 1)$e[3ga`tb"

# Row 24: sequences a terminal must treat as one character each (see the test for what happens).
Emit "$(At 24 1)end $heart $thumb $family $flag |$e[4 q"
[IO.File]::WriteAllText("$here\conformance.vt", $c.ToString(), $utf8)

# -------------------------------------------------- conformance.expect.txt
# Written from the specifications, row by row. Trailing blanks are not written.
$sp = { param($n) ' ' * $n }
$expect = @(
  "PROTEUS conformance sample (80x24): compare with conformance.expect.txt",   # 1
  "bold dim italic inverse hidden strike plain",                               # 2
  "U1 U2 U3 U4 U5 D xyzwv",                                                    # 3
  "abcdefghijklmnopqrstuv",                                                    # 4
  "H1AC4B6D8F I K M",                                                          # 5
  "G       E  JLN",                                                            # 6
  "a       b       c   F   g",                                                 # 7
  "日本語 한 $grin e$acute a$diaeresis$dotbelow .",                             # 8
  "$(& $sp 78)x",                                                              # 9
  "日  X本  Y",                                                                 # 10
  "a   XYcdeij        ta",                                                     # 11
  "      6789ABCDE",                                                           # 12
  "om",                                                                        # 13
  "   X",                                                                      # 14
  "r2",                                                                        # 15
  "il",                                                                        # 16
  "Z3",                                                                        # 17
  "guard: this row sits just below the scroll region and must not move",       # 18
  "alt:back",                                                                  # 19
  "$(& $sp 75)wrapp",                                                          # 20
  "ed$(& $sp 75)NOP",                                                          # 21
  "xxxx ┌──┐ link",                                                            # 22
  "a$(& $sp 78)b",                                                             # 23
  "end $heart $thumb $family $flag |"                                          # 24 (one cell pair per cluster)
)
[IO.File]::WriteAllText("$here\conformance.expect.txt", ($expect -join "`n") + "`n", $utf8)
"wrote draw.vt ($((Get-Item "$here\draw.vt").Length) bytes), conformance.vt ($((Get-Item "$here\conformance.vt").Length) bytes), conformance.expect.txt"
