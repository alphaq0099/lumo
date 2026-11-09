lumo — Windows x64 build
========================

Quick start
-----------
1. Put lumo.exe (and these files) anywhere you like, e.g. C:\Tools\lumo\
2. Double-click lumo.exe, or:

   lumo.exe C:\pics\a.jpg C:\pics\b.jpg comic.cbz scan.pdf

   Every file you pass opens in ONE window; navigation stays inside that
   selection (comic.cbz/scan.pdf open as albums with their own pages).

Multi-select from Explorer (right-click)
----------------------------------------
1. Edit lumo-open.reg: replace C:\Tools\lumo\lumo.exe with your real path.
2. Double-click the .reg (one time).
3. Select any files in Explorer -> right-click -> "Open with lumo".
   The whole selection opens in a single lumo window.

Or simply: select files and DRAG them onto the lumo window.

Keys
----
-> / Space      next page          <- / Backspace  previous page
PgUp / PgDn     previous / next FILE in the selection
Home / End      first / last page of current file
mouse wheel     zoom (at cursor)   drag            pan
0 / 1           fit / 100%         + / -           zoom in / out
F11 or F        fullscreen         Ctrl+O          open files
Ctrl+R / L      rotate             Ctrl+H / V      flip
Ctrl+X          crop (drag a box, Enter applies, Esc cancels)
Ctrl+Z          undo               Ctrl+C          copy image
Ctrl+S          save (keeps a one-time .original. backup)
Ctrl+Shift+S    save as

Comics
------
CBZ / ZIP / EPUB open directly. CBR / RAR work the same way (lumo reads them
on the fly; no unrar.exe needed).

PDF (optional)
--------------
PDF needs the small pdfium.dll next to lumo.exe (one time):
right-click get-pdfium.ps1 -> "Run with PowerShell", or:
  powershell -ExecutionPolicy Bypass -File get-pdfium.ps1
Without pdfium.dll everything else still works; PDF files show a hint.

Test images
-----------
img1..img5.png are included - try:
  lumo.exe img1.png img2.png img3.png comic.cbz
