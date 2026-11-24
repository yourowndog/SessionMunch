// Focused runtime regression for the generated V2 binding; no hub or model calls.
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const directory=await mkdtemp(join(tmpdir(),'sessionmunch-opencode2-'));
try {
  const binding=await readFile(new URL('../../crates/sessionmunch-cli/templates/plugins/opencode2_binding.ts',import.meta.url),'utf8');
  const prelude=`
  const handoffFetches=new Map(); const startedSessions=new Set();
  let fetched=0; let drained=0; const checkpoints=[];
  const cwdFor=(_id,directory)=>directory;
  const startSession=(id)=>{if(id)startedSessions.add(id)};
  const endSession=(id)=>startedSessions.delete(id);
  const postHook=()=>{}; const textFromParts=()=>'';
  const fetchHandoff=async()=>{fetched++;return 'stable handoff'};
  const postPreCompact=(id)=>checkpoints.push(id);
  const drainHookQueueForDispose=async()=>{drained++};
  export const state=()=>({fetched,drained,checkpoints});
  `;
  const file=join(directory,'binding.ts');await writeFile(file,prelude+binding);
  const {default:plugin,state}=await import(pathToFileURL(file));
  const callbacks={};let disposed=0;
  const hook=async(name,fn)=>{callbacks[name]=fn;return {dispose:async()=>{disposed++}}};
  const cleanup=await plugin.setup({location:{directory},event:{subscribe:()=>({async *[Symbol.asyncIterator](){}})},session:{hook},tool:{hook}});
  const first={sessionID:'fixture',system:[]};await Promise.all([callbacks.context(first),callbacks.context(first)]);
  const second={sessionID:'fixture',system:[]};await callbacks.context(second);
  await callbacks.context(second);
  assert.deepEqual(first.system,[{type:'text',text:'stable handoff'}]);
  assert.deepEqual(second.system,first.system);
  assert.equal(state().fetched,1,'fetch/claim once per session');
  const other={sessionID:'other',system:[]};await callbacks.context(other);
  assert.equal(state().fetched,2,'separate sessions do not share a fetch');
  await callbacks.context({sessionID:'fixture'});
  assert.equal(state().fetched,2,'invalid context is ignored');
  await cleanup();assert.equal(disposed,4);
  console.log('OpenCode2 stable recall, per-context dedup and cleanup PASS');
} finally {await rm(directory,{recursive:true,force:true})}
