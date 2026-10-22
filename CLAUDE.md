# CLAUDE.md

raterm : émulateur de terminal Linux en Rust dont la grille est dessinée par
GTK3 avec Pango, et qui affiche aussi des blocs riches (Markdown, HTML, SVG,
images, vidéos, audio), chacun dans une webview WebKitGTK, envoyés par la
séquence OSC 7117. `rat` est l'outil
compagnon qui émet ces blocs. La spec (pages `Spec*` du wiki, dépôt séparé
dans `.wiki`) décrit la conception et fait foi ; les autres pages du wiki et
le README sont la doc utilisateur.

Ce fichier est le seul en français : code, commentaires, README, wiki et
messages de commit sont en anglais.

## Organisation

- `crates/raterm/src/` : la bibliothèque (le cœur), testable sans fenêtre :
  `pty`, `term` (état via `alacritty_terminal`, écran rendu en runs, ce qui a
  changé à chaque lot), `history` (l'historique illimité), `rich` (filtre
  OSC 7117, rendu Markdown avec comrak + syntect), `media` (registre et
  serveur HTTP local des vidéos et de l'audio), `session`, `config`.
- `crates/raterm/src/native/` et `main.rs` : le binaire. `native` est la vue
  (grille dessinée par GTK/Pango, blocs en webviews enfantes, voir
  `Spec-Native-grid`), le seul module qui connaît la fenêtre, GTK et WebKit.
- `crates/rat/` : le second binaire, sans GTK ni WebKit.
- `crates/raterm/tests/` : tests snapshot rejouant des sessions enregistrées.

## Commandes

```sh
cargo build --workspace
cargo test --workspace                      # sans fenêtre
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
RATERM_UPDATE_SNAPSHOTS=1 cargo test --test snapshots   # réécrit les .out
```

## Conventions

- Rust stable, édition 2024 ; clippy et fmt propres à chaque commit.
- Concis plutôt qu'exhaustif ; commenter le pourquoi, pas le quoi.
- Les scripts de raterm dans les blocs (hauteur, surlignage de la recherche)
  ne construisent jamais de balisage avec le contenu du terminal, sans `eval`.
- Dépendances limitées à la liste de la spec (`.wiki/Spec.md`) ; toute
  nouvelle dépendance se justifie dans la spec et dans le commit.
- Une fonctionnalité hors spec s'écrit d'abord dans la spec du wiki. Une
  décision structurante (sécurité, architecture) se demande, elle ne se
  tranche pas seul.
- Commits atomiques, messages à l'impératif, sans trailer (pas de
  Co-Authored-By). Ne pousser que sur demande, `main` comme le wiki.
- Les `.in`/`.out` de `tests/data` sont des captures réelles : ne jamais les
  réécrire à la main, seulement régénérer les `.out` et relire le diff.

## Sécurité, à ne pas casser

- Chaque bloc est une webview dont le document est le bloc seul : ses
  scripts ne s'exécutent jamais (`set_enable_javascript_markup(false)`), et
  sa CSP (`default-src 'none'`) n'autorise que les styles inline, les images
  `data:` et le serveur média local ; le SVG est affiché en `<img>`.
- `raterm` n'accède jamais au réseau. C'est `rat --allow-network` qui
  télécharge, jamais `raterm`.
- Les liens s'ouvrent via `xdg-open` (http, https, mailto seulement), et
  seulement sur un clic (`NavigationType::LinkClicked`) : sans iframe
  sandboxée, un `<meta http-equiv="refresh">` ouvrirait sinon une page tout
  seul.
- Le serveur vidéo n'écoute que sur `127.0.0.1`, sous un jeton aléatoire, et
  ne sert que les fichiers annoncés par un bloc.

## Pièges de WebKitGTK déjà rencontrés

- Le lecteur vidéo (GStreamer) ne lit que http(s), `data:` et `blob:`, pas
  `file://` ; il faut `gst-plugins-good` et `gst-libav`.
- Sans le crate `javascriptcore`, on ne lit pas le résultat d'un script : la
  hauteur d'un bloc (`h:`) et la position d'une correspondance (`f:`)
  remontent par `document.title`.
- La recherche de WebKit (`FindController`) laissait des tranches de
  surlignage : les correspondances d'un bloc sont surlignées par un script
  de raterm (API CSS Custom Highlight), et une bascule d'opacité (1 ↔ 0,999)
  force la repeinture.
- WebKit rend translucide une couleur de sélection opaque : elle est en
  `rgba(…, 0.99)`. Un texte sélectionné se glissait comme une image :
  `-webkit-user-drag: none` et `drag_cancel`, plus `removeAllRanges` avant
  un clic.
- La webview d'un bloc est opaque : transparente, elle perdait le reste du
  bloc en repeignant une sélection.
- Les glyphes des polices de secours ont d'autres largeurs et hauteurs : la
  hauteur de ligne est fixée à la cellule mesurée, et hors ASCII chaque
  cellule est dessinée à sa place (`draw.rs`).
- Les noms de police doivent être entre guillemets en CSS (`Source Serif 4`).
- Sous Wayland avec Nvidia, le rendu DMA-BUF plante : `main.rs` pose
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`.

## Tester en vrai

Thomas utilise `raterm` au quotidien : ne jamais lancer `pkill raterm`,
`killall` ou `pgrep -x raterm`, qui fermeraient ses fenêtres. Pour un test :

- lancer `./target/debug/raterm --title raterm-test-<uuid> -e …` en
  arrière-plan, garder `$!`, retrouver la fenêtre avec `hyprctl clients -j`
  filtré sur ce pid, capturer avec `grim -g` sur sa géométrie exacte, puis
  `kill` ce pid seulement ;
- `SHELL=script.sh` ou `-e` permettent de piloter le contenu sans clavier ;
  ne jamais simuler de frappes avec `wtype`/`ydotool` (elles partent dans la
  fenêtre qui a le focus, pas forcément celle de test) ;
- pour inspecter un bloc, faire remonter une valeur par `document.title`
  (comme `h:`) et l'afficher avec un `eprintln!` temporaire. Restaurer
  ensuite ;
- `XDG_CONFIG_HOME` permet de tester une config isolée ; la config réelle de
  Thomas est dans `~/.config/raterm/`.
- Dans un pty de test, `timeout` doit avoir `--foreground` pour lire le
  terminal ; le shell de Thomas est zsh (pas de découpage des variables).
