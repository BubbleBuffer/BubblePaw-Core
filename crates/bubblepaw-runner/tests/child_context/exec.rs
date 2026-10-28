use super::*;

/// Writes a marker into the working directory, then prints 20,000 lines. The
/// full result crosses the inline limit and must spill.
#[cfg(windows)]
fn spill_command(marker: &str, scripts: &Path) -> Value {
    windows_command::powershell_file(
        scripts,
        &format!("spill-{marker}.ps1"),
        &format!(
            "[IO.File]::WriteAllText('{marker}','{marker}'); 0..19999 | ForEach-Object {{ 'line-{{0:D5}} payload' -f $_ }}"
        ),
    )
}
#[cfg(not(windows))]
fn spill_command(marker: &str, _scripts: &Path) -> Value {
    json!(format!(
        "printf '%s' '{marker}' > {marker}; awk 'BEGIN {{ for(i=0;i<20000;i++) printf \"line-%05d payload\\n\", i }}'"
    ))
}
#[cfg(windows)]
pub(super) fn long_command(marker: &str) -> String {
    format!("<nul set /p={marker}>{marker} & ping -n 31 127.0.0.1 >nul")
}
#[cfg(not(windows))]
pub(super) fn long_command(marker: &str) -> String {
    format!("printf '%s' '{marker}' > {marker}; sleep 30")
}
#[cfg(windows)]
pub(super) fn quick_command() -> &'static str {
    "echo ok"
}
#[cfg(not(windows))]
pub(super) fn quick_command() -> &'static str {
    "true"
}

pub(super) const SPILL_CODE: &str = "const rows=Array.from({length:6000},(_,i)=>'row-'+String(i).padStart(5,'0')+' '+'j'.repeat(40)); return {rows};";

/// Reads the external skill body and helper, runs a large exec, then pages its full
/// spill back through tools.read and returns a checksum of exactly what was read.
pub(super) fn work_code(
    tag: &str,
    body_literal: &str,
    helper_literal: &str,
    scripts: &Path,
) -> String {
    let command = json!(spill_command(&format!("cwd-{tag}.txt"), scripts));
    format!(
        "const body=await tools.read({{path:{body_literal}}}); \
const helper=await tools.read({{path:{helper_literal}}}); \
const run=await tools.exec({{command:{command}}}); \
if(run.exitCode!==0||!run.fullOutput) throw Error(JSON.stringify(run)); \
const path=run.fullOutput.path; let full='', start=1, pages=0; \
for(;;) {{ const page=await tools.read({{path,startLine:start,lineCount:1000}}); full+=page.content; pages++; \
if(page.returnedLines===0||start+page.returnedLines>page.totalLines) break; start+=page.returnedLines; }} \
let hash=0; for(let i=0;i<full.length;i++) hash=(hash*31+full.charCodeAt(i))%2147483647; \
return {{tag:'{tag}',external:{{body:body.content.includes('EXTERNAL-BODY-TOKEN'),helper:helper.content.includes('EXTERNAL-HELPER-TOKEN')}},exec:{{path,bytes:full.length,pages,hash}}}};"
    )
}
