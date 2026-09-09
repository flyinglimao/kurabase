import { chromium } from '@playwright/test';
import { spawn } from 'node:child_process';
import { open } from 'node:fs/promises';
import { startLocal, root, publishableKey, secretKey } from './local-stack.mjs';
import assert from 'node:assert/strict';

const stack=await startLocal({rpcPort:28545,gatewayPort:25432,adminPort:25433});
const servers=[];
let browser;
async function vite(app,port){
  const log=await open(`${root}/.runtime/${app}-browser.log`,'a');
  const child=spawn(process.execPath,[`${root}/apps/${app}/node_modules/vite/bin/vite.js`,'--host','127.0.0.1','--port',String(port),'--strictPort'],{cwd:`${root}/apps/${app}`,stdio:['ignore',log.fd,log.fd]});
  await log.close();servers.push(child);
  for(let i=0;i<100;i++){try{if((await fetch(`http://127.0.0.1:${port}`)).ok)return;}catch{}await new Promise(r=>setTimeout(r,100));}
  throw new Error(`Vite ${app} did not start`);
}
try{
  const seeded=await fetch(`${stack.adminUrl}/admin/v1/sql`,{method:'POST',headers:{apikey:secretKey,'Content-Type':'application/json'},body:JSON.stringify({sql:"CREATE TABLE browser_posts(id integer PRIMARY KEY,title text NOT NULL); INSERT INTO browser_posts VALUES(1,'Browser verified');"})});
  assert.equal(seeded.status,200,await seeded.text());
  await Promise.all([vite('dashboard',25173),vite('docs',25174)]);
  browser=await chromium.launch({channel:'chrome',headless:true});
  const context=await browser.newContext({viewport:{width:1440,height:1000}});
  const page=await context.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto('http://127.0.0.1:25173');
  await page.getByLabel('Gateway URL').fill(stack.adminUrl);
  await page.getByLabel('Publishable key',{exact:true}).fill(publishableKey);
  await page.getByLabel('Wallet or identity session token (optional, memory only)').fill('browser-session-must-not-persist');
  await page.getByLabel('Secret server key (memory only)').fill(secretKey);
  await page.getByRole('button',{name:'Connect gateway',exact:true}).click();
  await page.getByText('Gateway connected',{exact:true}).waitFor();
  await page.screenshot({path:`${root}/.runtime/dashboard-overview.png`,fullPage:true});
  await page.getByRole('button',{name:'tables',exact:true}).click();
  await page.getByRole('button',{name:'public.browser_posts',exact:true}).click();
  await page.getByRole('button',{name:'Load first 50 rows',exact:true}).click();
  await page.getByText(/Browser verified/).waitFor();
  await page.getByRole('button',{name:'sql',exact:true}).click();
  await page.locator('textarea').fill("UPDATE browser_posts SET title='Edited in dashboard' WHERE id=1 RETURNING *");
  await page.getByRole('button',{name:'Run SQL',exact:true}).click();
  await page.getByText(/Edited in dashboard/).waitFor();
  await page.screenshot({path:`${root}/.runtime/dashboard-sql.png`,fullPage:true});
  const saved=await page.evaluate(()=>JSON.stringify(localStorage));assert.ok(!saved.includes(secretKey),'Secret key leaked to local storage');assert.ok(!saved.includes('browser-session-must-not-persist'),'Session bearer leaked to local storage');
  await page.reload();await page.getByLabel('Secret server key (memory only)').waitFor();assert.equal(await page.getByLabel('Secret server key (memory only)').inputValue(),'');assert.equal(await page.getByLabel('Wallet or identity session token (optional, memory only)').inputValue(),'');
  await page.setViewportSize({width:390,height:844});await page.screenshot({path:`${root}/.runtime/dashboard-mobile.png`,fullPage:true});
  const overflows=await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth);assert.equal(overflows,false,'Mobile viewport overflows');
  await page.goto('http://127.0.0.1:25174');await page.screenshot({path:`${root}/.runtime/docs-mobile.png`,fullPage:true});
  assert.match(await page.title(),/Kurabase/i);
  const llms=await fetch('http://127.0.0.1:25174/llms-full.txt');assert.equal(llms.status,200);assert.match(await llms.text(),/createClient/);
  assert.deepEqual(errors,[]);
  console.log('Browser smoke passed: instance connection, real table read, on-chain SQL update, secret non-persistence, mobile layout, docs/LLM reference. Screenshots: .runtime/*.png');
}catch(error){
  const page=browser?.contexts()[0]?.pages()[0];
  if(page){console.error((await page.locator('body').innerText()).slice(0,6000));await page.screenshot({path:`${root}/.runtime/browser-failure.png`,fullPage:true});}
  throw error;
}finally{
  await browser?.close();
  for(const server of servers)server.kill('SIGTERM');
  await stack.stop();
}
