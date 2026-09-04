// Bundle the guest programs:
//   - dist/character.js: plain TS policy bundle (no JSX/pak needed).
//   - dist/menu.js + dist/menu.pak: the PocketUI TSX app, built through the
//     vendored PocketJS pipeline (tools/build.ts --outdir keeps outputs
//     repo-local; see that script's header). Bun regenerates the bundle/pak;
//     at runtime the Rust binary only loads the generated artifacts.
export {};

const result = await Bun.build({
  entrypoints: ["app/main.ts"],
  outdir: "dist",
  naming: "character.[ext]",
  format: "iife",
  target: "browser",
  minify: false,
});
if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}
console.log("dist/character.js built");

const menu = Bun.spawnSync(
  // Keep menu raster assets native through the desktop's 1x-2x scale range.
  ["bun", "vendor/pocketjs/tools/build.ts", "app/menu.tsx", "--density=2", "--outdir=dist"],
  { stdout: "inherit", stderr: "inherit" },
);
if (!menu.success) {
  console.error("menu build failed — run `bun install` in vendor/pocketjs first?");
  process.exit(1);
}
