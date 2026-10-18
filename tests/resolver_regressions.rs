use libdeno::{run, run_with, LibdenoOptions, LibdenoRuntime};
use std::fs;

#[test]
fn npm_and_jsr_prefix_import_maps_resolve_static_and_variable_dynamic_imports() {
    let dir = std::env::temp_dir().join(format!("libdeno-prefix-map-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("deno.json"),
        r#"{"lock":false,"imports":{"npm:local-shim":"./shim.js","jsr:local-shim":"./shim.js"}}"#,
    )
    .unwrap();
    fs::write(dir.join("shim.js"), "export const value = 'local-shim';").unwrap();
    fs::write(dir.join("main.js"), r#"
import { value as npmStatic } from 'npm:local-shim';
import { value as jsrStatic } from 'jsr:local-shim';
const scheme = 'npm';
const jsrScheme = 'jsr';
const npmDynamic = await import(`${scheme}:local-shim`);
const jsrDynamic = await import(`${jsrScheme}:local-shim`);
if ([npmStatic, jsrStatic, npmDynamic.value, jsrDynamic.value].some(value => value !== 'local-shim')) throw Error('prefix import map failed');
"#).unwrap();
    let result = run(
        dir.join("main.js"),
        &LibdenoOptions {
            cwd: Some(dir.clone()),
            allow_all_permissions: true,
            features: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result, 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn jsx_workspace_defaults_and_file_pragma_select_local_runtimes() {
    let dir = std::env::temp_dir().join(format!("libdeno-jsx-mapping-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("runtime")).unwrap();
    fs::create_dir_all(dir.join("pragma")).unwrap();
    fs::write(dir.join("deno.json"), r#"{"lock":false,"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"./runtime","jsxImportSourceTypes":"./runtime-types"}}"#).unwrap();
    for (path, value) in [
        ("runtime/jsx-runtime", "workspace"),
        ("pragma/jsx-runtime", "pragma"),
    ] {
        fs::write(dir.join(path), format!("export const jsx=()=>({{value:{value:?}}}); export const jsxs=jsx; export const Fragment='fragment';")).unwrap();
    }
    fs::write(
        dir.join("pragma-case.tsx"),
        "/** @jsxImportSource ./pragma */\nexport const value = <div />;",
    )
    .unwrap();
    fs::write(
        dir.join("default-case.tsx"),
        "export const value = <div />;",
    )
    .unwrap();
    fs::write(dir.join("main.js"), r#"
import { value as pragma } from './pragma-case.tsx';
import { value as workspace } from './default-case.tsx';
if (pragma.value !== 'pragma' || workspace.value !== 'workspace') throw Error('JSX import source mapping failed');
"#).unwrap();
    assert_eq!(
        run(
            dir.join("main.js"),
            &LibdenoOptions {
                cwd: Some(dir.clone()),
                allow_all_permissions: true,
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
    fs::create_dir_all(dir.join("dev-runtime")).unwrap();
    fs::write(
        dir.join("dev-runtime/jsx-dev-runtime"),
        "export const jsxDEV=()=>({value:'dev'}); export const Fragment='fragment';",
    )
    .unwrap();
    fs::write(dir.join("deno.json"), r#"{"lock":false,"compilerOptions":{"jsx":"react-jsxdev","jsxImportSource":"./dev-runtime"}}"#).unwrap();
    fs::write(dir.join("dev-case.tsx"), "export const value = <div />;").unwrap();
    fs::write(dir.join("main.js"), "import {value} from './dev-case.tsx'; if(value.value!=='dev') throw Error('JSX dev runtime mapping failed');").unwrap();
    assert_eq!(
        run(
            dir.join("main.js"),
            &LibdenoOptions {
                cwd: Some(dir.clone()),
                allow_all_permissions: true,
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn reusable_runtime_rebuilds_after_import_map_symlink_retarget() {
    use std::os::unix::fs::symlink;
    let dir = std::env::temp_dir().join(format!("libdeno-map-link-reuse-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("deno.json"),
        r#"{"lock":false,"importMap":"./map.json"}"#,
    )
    .unwrap();
    fs::write(dir.join("map-a.json"), r#"{"imports":{"alias":"./a.js"}}"#).unwrap();
    fs::write(dir.join("map-b.json"), r#"{"imports":{"alias":"./b.js"}}"#).unwrap();
    fs::write(dir.join("a.js"), "export const value='a';").unwrap();
    fs::write(dir.join("b.js"), "export const value='b';").unwrap();
    fs::write(dir.join("main.js"), "import {value} from 'alias'; Deno.writeTextFileSync(new URL('./out.txt',import.meta.url),value);").unwrap();
    symlink(dir.join("map-a.json"), dir.join("map.json")).unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = tokio.block_on(LibdenoRuntime::new(&dir)).unwrap();
    drop(tokio);
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(
        run_with(&runtime, dir.join("main.js"), &options).unwrap(),
        0
    );
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "a");
    fs::remove_file(dir.join("map.json")).unwrap();
    symlink(dir.join("map-b.json"), dir.join("map.json")).unwrap();
    assert_eq!(
        run_with(&runtime, dir.join("main.js"), &options).unwrap(),
        0
    );
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "b");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn failed_bare_dependency_reloads_its_importer_on_retry() {
    let dir = std::env::temp_dir().join(format!("libdeno-bare-retry-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("deno.json"),
        r#"{"lock":false,"nodeModulesDir":"manual"}"#,
    )
    .unwrap();
    fs::write(
        dir.join("root.js"),
        "import { value } from './middle.js'; export { value };",
    )
    .unwrap();
    fs::write(
        dir.join("middle.js"),
        "import { value } from 'latebarepkg'; export { value };",
    )
    .unwrap();
    fs::write(dir.join("main.js"), r#"
try { await import('./root.js?first'); throw Error('missing bare package unexpectedly resolved'); }
catch (error) {
  if (!String(error).includes('latebarepkg')) throw error;
  Deno.mkdirSync(new URL('./node_modules/latebarepkg/', import.meta.url), { recursive: true });
  Deno.writeTextFileSync(new URL('./node_modules/latebarepkg/package.json', import.meta.url), '{"name":"latebarepkg","type":"module","exports":"./index.js"}');
  Deno.writeTextFileSync(new URL('./node_modules/latebarepkg/index.js', import.meta.url), 'export const value = 42;');
}

const result = await import('./root.js?second');
if (result.value !== 42) throw Error('retry did not resolve newly-created package');
"#).unwrap();
    assert_eq!(
        run(
            dir.join("main.js"),
            &LibdenoOptions {
                cwd: Some(dir.clone()),
                allow_all_permissions: true,
                features: Some(vec![]),
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
    let _ = fs::remove_dir_all(dir);
}
