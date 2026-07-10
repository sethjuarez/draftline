import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";

const [, , packageDir, packageName, outputDir, ...dependencyMappings] = process.argv;

if (!packageDir || !packageName || !outputDir) {
  console.error(
    "Usage: node scripts/stage-github-package.mjs <package-dir> <package-name> <output-dir> [dependency=value ...]",
  );
  process.exit(1);
}

const root = process.cwd();
const sourceDir = path.resolve(root, packageDir);
const targetDir = path.resolve(root, outputDir);
const packageJsonPath = path.join(sourceDir, "package.json");
const packageJson = JSON.parse(await readFile(packageJsonPath, "utf8"));

const dependencyOverrides = new Map();
for (const mapping of dependencyMappings) {
  const separatorIndex = mapping.indexOf("=");
  if (separatorIndex <= 0 || separatorIndex === mapping.length - 1) {
    console.error(`Invalid dependency mapping: ${mapping}`);
    process.exit(1);
  }

  dependencyOverrides.set(mapping.slice(0, separatorIndex), mapping.slice(separatorIndex + 1));
}

packageJson.name = packageName;
packageJson.publishConfig = {
  ...packageJson.publishConfig,
  access: "public",
  registry: "https://npm.pkg.github.com",
};

for (const [dependencyName, dependencyVersion] of dependencyOverrides) {
  if (!packageJson.dependencies?.[dependencyName]) {
    console.error(`${dependencyName} is not a dependency of ${packageDir}`);
    process.exit(1);
  }

  packageJson.dependencies[dependencyName] = dependencyVersion;
}

await rm(targetDir, { force: true, recursive: true });
await mkdir(targetDir, { recursive: true });
await writeFile(path.join(targetDir, "package.json"), `${JSON.stringify(packageJson, null, 2)}\n`);

for (const entry of packageJson.files ?? []) {
  await cp(path.join(sourceDir, entry), path.join(targetDir, entry), {
    errorOnExist: false,
    force: true,
    recursive: true,
  });
}
