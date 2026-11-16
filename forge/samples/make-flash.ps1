# Writes the flash-safety stress samples (played by `proteus shot --flash`, see src/flash.rs):
#   red-flood.vt   240 lines of failing builds and tests: theme red, bold red, bright red,
#                  24-bit saturated red, 256-colour red, red labels; mixed with plain lines
#   flash-4hz.vt   two chunks, a full screen of saturated red and a full screen of the
#                  ground colour; played one chunk every 7.5 frames at 60 fps it alternates
#                  at 4 Hz, which the analyser must flag (the positive control)
#   pwsh forge\samples\make-flash.ps1
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$e = [char]27
$utf8 = [Text.UTF8Encoding]::new($false)

$lines = for ($i = 0; $i -lt 240; $i++) {
  $n = 10 + ($i * 7) % 90
  switch ($i % 12) {
    0 { "$e[1;31merror[E0$(300 + $i % 90)]$e[0m$e[1m: mismatched types$e[0m" }
    1 { "$e[34m   --> $e[0msrc/module_$($i % 17).rs:${n}:$(($i * 3) % 40 + 1)" }
    2 { "$e[34m    |$e[0m" }
    3 { "$e[34m$('{0,3}' -f $n) |$e[0m     let value: u32 = compute_$i(input, &config).await?;" }
    4 { "$e[34m    |$e[0m                      $e[1;31m^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected ``u32``, found ``Result<u64, Error>``$e[0m" }
    5 { "$e[38;2;255;0;0mFAILED tests::case_$i ... panicked at 'assertion failed: left == right' (left: $n, right: $($n + 1))$e[0m" }
    6 { "$e[38;5;196mthread 'worker-$($i % 8)' panicked at src/pool.rs:${n}:9: called ``Option::unwrap()`` on a ``None`` value$e[0m" }
    7 { "$e[41;97m FAIL $e[0m src/components/widget_$i.test.tsx $e[31m($n ms)$e[0m" }
    8 { "$e[31m  x expected $n to equal $($n * 2); the snapshot differs in $($i % 5 + 1) places$e[0m" }
    9 { "$e[1;31merror$e[0m: could not compile ``crate_$($i % 9)`` (lib) due to $($i % 7 + 1) previous errors" }
    10 { "$e[91mERR! $e[0mcode ELIFECYCLE errno $n at step $i" }
    default { "   Compiling dependency_$i v0.$($i % 9).$n ($e[31m$($i % 3) warnings$e[0m)" }
  }
}
[IO.File]::WriteAllText("$here\red-flood.vt", ($lines -join "`r`n") + "`r`n", $utf8)
[IO.File]::WriteAllText("$here\flash-4hz.vt", "$e[48;2;255;0;0m$e[2J$e[H`n$e[48;2;5;7;13m$e[2J$e[H`n", $utf8)
