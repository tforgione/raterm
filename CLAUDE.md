# CLAUDE.md

raterm : émulateur de terminal Linux en Rust dont l'écran est une webview
(wry/tao sur WebKitGTK), qui affiche aussi des blocs riches (Markdown, HTML,
SVG, images, vidéos) envoyés par la séquence OSC 7117. `rat` est l'outil
compagnon qui émet ces blocs. La spec (pages `Spec*` du wiki, dépôt séparé
dans `.wiki`) décrit la conception et fait foi ; les autres pages du wiki et
le README sont la doc utilisateur.

Ce fichier est le seul en français : code, commentaires, README, wiki et
messages de commit sont en anglais.

## Organisation

- `crates/raterm/src/` : la bibliothèque (le cœur), testable sans fenêtre :
  `pty`, `term` (état via `alacritty_terminal`, diff d'écran, événements),
  `rich` (filtre OSC 7117, rendu Markdown avec comrak + syntect), `media`
  (registre et serveur HTTP local des vidéos), `session`, `config`.
- `crates/raterm/src/view.rs` et `main.rs` : le binaire. `view` est le seul
  module qui connaît wry/tao.
- `crates/raterm/assets/` : la page (`index.html`, `style.css`, `app.js`),
  embarquée dans le binaire et servie sous `raterm://app/`.
- `crates/rat/` : le second binaire, sans wry.
- `crates/raterm/tests/` : tests snapshot rejouant des sessions enregistrées.

## Commandes

```sh
cargo build --workspace
cargo test --workspace                      # sans fenêtre
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
RATERM_UPDATE_SNAPSHOTS=1 cargo test --test snapshots   # réécrit les .out
```

Après un changement dans `assets/`, il faut recompiler : la page est incluse
dans le binaire.

## Conventions

- Rust stable, édition 2024 ; clippy et fmt propres à chaque commit.
- Concis plutôt qu'exhaustif ; commenter le pourquoi, pas le quoi.
- Page en HTML/CSS/JS vanilla, un fichier par type, sans dépendance, sans
  `eval`, sans `innerHTML` avec du contenu du terminal hors des iframes.
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

- Les blocs HTML/Markdown sont dans des `<iframe sandbox>` sans
  `allow-scripts` ; le SVG est affiché en `<img>`.
- `raterm` n'accède jamais au réseau : la CSP de la page n'autorise que les
  images `data:` et le serveur vidéo local. C'est `rat --allow-network` qui
  télécharge, jamais `raterm`.
- Les liens s'ouvrent via `xdg-open` (http, https, mailto seulement).
- Le serveur vidéo n'écoute que sur `127.0.0.1`, sous un jeton aléatoire, et
  ne sert que les fichiers annoncés par un bloc.

## Pièges de WebKitGTK déjà rencontrés

- WebKit n'exécute aucun écouteur sur un document sandboxé sans scripts, même
  ajouté par la page parente : clics et touches dans un bloc passent par le
  gestionnaire de navigation (liens) ou par la reprise du focus (`blur`).
- La CSP s'applique aux navigations des iframes avant le gestionnaire de
  navigation de wry (d'où `frame-src http: https: mailto:`).
- Le lecteur vidéo (GStreamer) ne lit que http(s), `data:` et `blob:`, ni
  `raterm://` ni `file://` ; il faut `gst-plugins-good` et `gst-libav`.
- Une iframe dont le `color-scheme` diffère de la page reçoit un fond opaque.
- Les glyphes des polices de secours ont d'autres largeurs et hauteurs : la
  hauteur de ligne est fixée à la cellule mesurée, et les glyphes qui ne
  tiennent pas sont mis en boîte (`cellText` dans `app.js`).
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
- pour inspecter la page, ajouter temporairement un
  `send({ type: "debug|…" })` dans `app.js` : le message apparaît dans
  l'erreur « unknown variant » sur la sortie d'erreur. Restaurer ensuite ;
- `XDG_CONFIG_HOME` permet de tester une config isolée ; la config réelle de
  Thomas est dans `~/.config/raterm/`.
- Dans un pty de test, `timeout` doit avoir `--foreground` pour lire le
  terminal ; le shell de Thomas est zsh (pas de découpage des variables).
