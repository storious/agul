Output docs/assets/agul-demo.gif

Require agul-demo
Set Shell "bash"
Set Width 1200
Set Height 720
Set FontSize 20
Set TypingSpeed 22ms
Set Framerate 20
Set PlaybackSpeed 1.0
Set WaitTimeout 20s
Set Theme { "name": "Agul", "black": "#05070b", "red": "#ff6b6b", "green": "#69d28f", "yellow": "#ffd75f", "blue": "#5fafff", "magenta": "#af87ff", "cyan": "#5fd7d7", "white": "#d9e7ff", "brightBlack": "#2b3340", "brightRed": "#ff8787", "brightGreen": "#87e0a5", "brightYellow": "#ffe083", "brightBlue": "#75baff", "brightMagenta": "#c29aff", "brightCyan": "#83e0e0", "brightWhite": "#f3f7ff", "background": "#05070b", "foreground": "#d9e7ff", "selectionBackground": "#16304f", "cursorAccent": "#05070b", "cursor": "#ffd75f" }

Hide
Type "clear"
Enter
Type "agul-demo"
Enter
Wait+Screen /local-fixture/
Show
Sleep 400ms

Type "@skill:project-summary"
Sleep 350ms
Type " Update STATUS.md from PROJECT.md, then verify it."
Enter

Wait+Screen /Done: STATUS.md is updated and verified./
Sleep 1400ms
Type "/exit"
Enter
Sleep 400ms
