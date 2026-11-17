# The screen's typeface (#618, #637)

`Antonio-Bold.ttf`: Antonio Bold by Vernon Adams, the static TTF from
<https://github.com/googlefonts/antonioFont> at commit
`08562998b10c6c21a3505f47236b42f5d58c9909` (`Fonts/TTF/Antonio-Bold.ttf`, the
commit Google Fonts' `ofl/antonio/METADATA.pb` records), unmodified. It lives
in `stormblock-screen-spec` (#637) so stormblock's screen and stormbootx's
panel draw from the one file; it was `src/screen/fonts` (#618).

- sha256 `f34e6f91b7cfd5d1d8e4fb2784d8ce9b198ca682014fe0f0886e51c0381b0d4d`, 55416 bytes
  (checked by the crate's test `the_font_is_the_pinned_one`)
- Licence: SIL Open Font License 1.1 (`OFL.txt`, from the same commit).
  Embedding the unmodified font in a program is permitted; it is not sold
  on its own and its name is not used for a modified version.
