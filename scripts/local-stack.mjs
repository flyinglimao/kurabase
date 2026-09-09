import { spawn } from 'node:child_process';
import { readFile, mkdir, open } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { ContractFactory, HDNodeWallet, JsonRpcProvider, ZeroAddress, ZeroHash, zeroPadValue } from 'ethers';

export const root = fileURLToPath(new URL('../',import.meta.url));
export const mnemonic = 'test test test test test test test test test test test junk';
export const publishableKey = 'kura_pub_local_anvil';
export const secretKey = 'kura_secret_local_anvil';
export const wallet = index => HDNodeWallet.fromPhrase(mnemonic,undefined,`m/44'/60'/0'/0/${index}`);
const wait = ms => new Promise(resolve=>setTimeout(resolve,ms));
async function ready(url, child) {
  for (let i=0;i<200;i++) {
    if (child.exitCode !== null) throw new Error(`Child exited ${child.exitCode} while waiting for ${url}`);
    try { const r=await fetch(url,{signal:AbortSignal.timeout(500)}); if(r.ok) return; } catch {}
    await wait(100);
  }
  throw new Error(`Timed out waiting for ${url}; inspect .runtime logs`);
}

export async function startLocal({rpcPort=18545, gatewayPort=15432, adminPort=15433}={}) {
  await mkdir(new URL('../.runtime/',import.meta.url),{recursive:true});
  const processes=[];
  let provider;
  async function launch(command,args,env,label) {
    const log=await open(new URL(`../.runtime/${label}.log`,import.meta.url),'a');
    const child=spawn(command,args,{cwd:root,env:{...process.env,...env},stdio:['ignore',log.fd,log.fd]});
    await log.close();
    processes.push(child);
    return child;
  }
  async function stopChild(child) {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited=new Promise(resolve=>child.once('exit',resolve));
    child.kill('SIGTERM');
    await Promise.race([exited,wait(3000)]);
    if(child.exitCode===null && child.signalCode===null) { child.kill('SIGKILL'); await exited; }
  }
  const stop=async()=>{ provider?.destroy(); for(const child of [...processes].reverse()) await stopChild(child); };
  const rpcUrl=`http://127.0.0.1:${rpcPort}`, url=`http://127.0.0.1:${gatewayPort}`, adminUrl=`http://127.0.0.1:${adminPort}`;
  try {
    const anvil=await launch('anvil',['--port',String(rpcPort),'--chain-id','31337','--gas-limit','100000000'],{},'anvil');
    provider=new JsonRpcProvider(rpcUrl,31337,{staticNetwork:true});
    for(let i=0;i<100;i++) { try { await provider.getBlockNumber(); break; } catch { if(anvil.exitCode!==null) throw new Error('Anvil failed'); await wait(100); } }
    const owner=await provider.getSigner(0);
    const artifact=JSON.parse(await readFile(new URL('../contracts/out/KurabaseSchema.sol/KurabaseSchema.json',import.meta.url),'utf8'));
    const contract=await new ContractFactory(artifact.abi,artifact.bytecode.object,owner).deploy(await owner.getAddress(),ZeroAddress);
    const receipt=await contract.deploymentTransaction().wait();
    await (await contract.setGateway(wallet(1).address,true,false)).wait();
    const address=await contract.getAddress();
    const baseEnv={KURA_RPC_URL:rpcUrl,KURA_CONTRACT:address,KURA_DEPLOYMENT_BLOCK:String(receipt.blockNumber),KURA_PUBLISHABLE_KEY:publishableKey,KURA_SECRET_KEY:secretKey};
    let gateway=await launch('./target/debug/kura-gateway',[],{...baseEnv,KURA_PRIVATE_KEY:wallet(1).privateKey,KURA_BIND:`127.0.0.1:${gatewayPort}`},'gateway');
    const admin=await launch('./target/debug/kura-gateway',[],{...baseEnv,KURA_PRIVATE_KEY:wallet(0).privateKey,KURA_BIND:`127.0.0.1:${adminPort}`},'admin-gateway');
    await Promise.all([ready(`${url}/health`,gateway),ready(`${adminUrl}/health`,admin)]);
    const restartGateway=async()=>{ await stopChild(gateway); gateway=await launch('./target/debug/kura-gateway',[],{...baseEnv,KURA_PRIVATE_KEY:wallet(1).privateKey,KURA_BIND:`127.0.0.1:${gatewayPort}`},'gateway'); await ready(`${url}/health`,gateway); };
    async function session(index=2) {
      const user=wallet(index);
      const latest=await provider.getBlock('latest');
      const grant=await contract.gateways(wallet(1).address);
      const value={gateway:wallet(1).address,user:user.address,uid:zeroPadValue(user.address,32),claimsHash:ZeroHash,expiresAt:String(latest.timestamp+86400),nonce:'0',gatewayEpoch:grant.epoch.toString()};
      const types={Session:[{name:'gateway',type:'address'},{name:'user',type:'address'},{name:'uid',type:'bytes32'},{name:'claimsHash',type:'bytes32'},{name:'expiresAt',type:'uint256'},{name:'nonce',type:'uint256'},{name:'gatewayEpoch',type:'uint256'}]};
      const signature=await user.signTypedData({name:'KurabaseSchema',version:'1',chainId:31337,verifyingContract:address},types,value);
      const envelope={session:value,signature,claims:null};
      const response=await fetch(`${url}/auth/v1/session`,{method:'POST',headers:{apikey:publishableKey,'Content-Type':'application/json'},body:JSON.stringify(envelope)});
      const result=await response.json();
      if(!response.ok) throw new Error(`Session exchange failed: ${JSON.stringify(result)}`);
      return {...result,envelope};
    }
    return {url,adminUrl,rpcUrl,provider,contract,address,stop,restartGateway,session};
  } catch(error) { await stop(); throw error; }
}
