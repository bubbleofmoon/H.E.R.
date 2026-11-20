import fs from 'node:fs/promises';
import path from 'node:path';

const escape = value => String(value).replace(/[&<>"']/g,
  character => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[character]);

export async function writeReport(directory, report) {
  await fs.writeFile(path.join(directory, 'report.json'), JSON.stringify(report, null, 2) + '\n');
  const screenshot = ({ browser, size, route, theme, status, file, detail }) => {
    const title = [browser, size, route, theme].filter(Boolean).join(' · ');
    return `<figure><figcaption>${escape(title)} · ${escape(status)}</figcaption>${file
      ? `<a href="${escape(file)}"><img loading="lazy" alt="${escape(title)}" src="${escape(file)}"></a>`
      : ''}${detail ? `<p>${escape(detail)}</p>` : ''}</figure>`;
  };
  const row = result => `<tr><th scope="row">${escape(result.id)}<br>${escape(result.label)}</th>
    <td>${escape(result.area)} / ${escape(result.section)}</td><td>${escape(result.status)}</td><td>${result.detail
      ? escape(result.detail) : `<ul>${result.checks
      .map(check => `<li>${escape([check.source, check.browser, check.size, check.status, check.detail].filter(Boolean).join(' · '))}</li>`)
      .join('')}</ul>`}</td></tr>`;
  const counts = Object.entries(Object.groupBy(report.functions, result => result.status))
    .map(([status, rows]) => `${rows.length} ${status}`).join(' · ');
  const html = `<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>HER console UI report</title>
<style>
body{font:16px system-ui;margin:24px;background:#f6f7f9;color:#172234}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,260px),1fr));gap:12px}
figure{margin:0;border:1px solid #cbd2dd;background:white;padding:8px;overflow-wrap:anywhere}
img{display:block;width:100%;height:auto;margin-top:8px}
table{border-collapse:collapse;width:100%;text-align:left;background:white}
th,td{border:1px solid #cbd2dd;padding:8px;vertical-align:top;overflow-wrap:anywhere}
ul{margin:0;padding-left:1.25em}
</style>
<h1>HER console UI report</h1>
<p>${escape(report.commit)} · ${escape(report.platform)} · ${escape(counts)}</p>
<p>${escape(Object.entries(report.browsers).map(([name, version]) => `${name} ${version}`).join(' · '))}</p>
<p><a href="report.json">JSON report</a> · <a href="#functions">Function walk</a></p>
<ul>${report.errors.map(error => `<li>${escape(error)}</li>`).join('')}</ul>
<h2>Screenshots</h2><div class="grid">${report.screenshots.map(screenshot).join('')}</div>
<h2 id="functions">Function walk</h2><table>
<thead><tr><th>Function</th><th>Shell placement</th><th>Result</th><th>Checks</th></tr></thead>
<tbody>${report.functions.map(row).join('')}</tbody></table></html>
`;
  await fs.writeFile(path.join(directory, 'index.html'), html);
}
