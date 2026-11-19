import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const guard=readFileSync(new URL('../src/sites/site_domain_guard.js',import.meta.url),'utf8');
const learning=readFileSync(new URL('../src/sites/learning.rs',import.meta.url),'utf8');
const expression=(domains,body)=>`(async()=>{ (${guard})(${JSON.stringify(domains)}, "x"); return await (${body}); })()`;
const context=href=>{const location=new URL(href);return vm.createContext({window:{location},location,URL});};

test('the production path guards before source evaluation in the same request',()=>{
 assert(learning.includes('include_str!("site_domain_guard.js")'));
 assert(learning.includes('({guard})({domains}, {site}); return await ({expression})'));
 const run=learning.slice(learning.indexOf('pub async fn run_site_browser_tool('),learning.indexOf('impl LoadedSiteSkill'));
 assert(!run.includes('page_info('));
 assert(!run.includes('ensure_page_matches_manifest('));
 assert(run.includes('page.evaluate_action(&expression)'));
 assert(run.includes('page.evaluate_json(&expression)'));
 assert(run.includes('validate_value_against_schema('));
 assert(run.includes('"match_ms": Value::Null'));
});

test('exact hosts normalize case and trailing dots; lookalikes remain blocked',async()=>{
 for(const href of ['https://x.com/post','https://X.COM./post','https://x.com:443/post']){
  assert.equal(await vm.runInContext(expression(['x.com'],'42'),context(href)),42);
 }
 for(const href of ['https://x.com.evil.test','https://evilx.com','https://www.x.com','https://x.com@evil.test','about:blank','data:text/plain,x.com']){
  const c=context(href);
  await assert.rejects(vm.runInContext(expression(['x.com'],'(globalThis.executed=true)'),c),/outside site skill/);
  assert.equal(c.executed,undefined);
 }
});

test('wildcards require a subdomain and never match the apex or suffix lookalikes',async()=>{
 for(const href of ['https://a.example.com','https://a.b.EXAMPLE.COM.']){
  assert.equal(await vm.runInContext(expression(['*.example.com'],'true'),context(href)),true);
 }
 for(const href of ['https://example.com','https://example.com.evil.test','https://evil-example.com']){
  await assert.rejects(vm.runInContext(expression(['*.example.com'],'true'),context(href)),/outside site skill/);
 }
});

test('a redirected renderer is checked anew, before bundle initialization',async()=>{
 const c=context('https://x.com');
 const body='(async()=>{globalThis.bundleLoaded=true;return {ok:true};})()';
 assert.equal((await vm.runInContext(expression(['x.com'],body),c)).ok,true);
 delete c.bundleLoaded;c.location.href='https://evil.test';
 await assert.rejects(vm.runInContext(expression(['x.com'],body),c),/outside site skill/);
 assert.equal(c.bundleLoaded,undefined);
});

test('a page replacing the URL constructor cannot spoof the domain guard',async()=>{
 const c=context('https://evil.test');
 c.URL=function(){return {hostname:'x.com'};};
 await assert.rejects(vm.runInContext(expression(['x.com'],'(globalThis.executed=true)'),c),/outside site skill/);
 assert.equal(c.executed,undefined);
});

test('page-modified String and Array prototype methods cannot permit a wrong host',async()=>{
 const c=context('https://evil.test');
 vm.runInContext('String.prototype.toLowerCase=()=>"x.com";String.prototype.endsWith=()=>true;String.prototype.replace=()=>"x.com";Array.prototype.some=()=>true;',c);
 await assert.rejects(vm.runInContext(expression(['x.com','*.x.com'],'(globalThis.executed=true)'),c),/outside site skill/);
 assert.equal(c.executed,undefined);
});

test('read values, arguments, action closures and failures retain their contracts',async()=>{
 const c=context('https://x.com');
 c.socaiAction=async perform=>perform();
 assert.equal(await vm.runInContext(expression(['x.com'],'(async()=>{return await socaiAction(()=>7);})()'),c),7);
 const value=await vm.runInContext(expression(['x.com'],'(async()=>{const tool=(args)=>args;return tool({count:0,empty:""});})()'),c);
 assert.equal(JSON.stringify(value),JSON.stringify({count:0,empty:''}));
 await assert.rejects(vm.runInContext(expression(['x.com'],'(async()=>{throw Error("tool failed");})()'),c),/tool failed/);
});
