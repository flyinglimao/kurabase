import {startLocal,publishableKey,secretKey} from './local-stack.mjs';
const stack=await startLocal({rpcPort:8545,gatewayPort:54321,adminPort:54322});
const session=await stack.session();
console.log(JSON.stringify({gateway:stack.url,adminGateway:stack.adminUrl,rpc:stack.rpcUrl,contract:stack.address,publishableKey,secretKey,localTestSession:session.access_token},null,2));
console.log('Local Anvil credentials only. Dashboard: pnpm --filter @kurabase/dashboard dev. Ctrl-C stops this stack.');
const stop=async()=>{await stack.stop();process.exit(0);};
process.on('SIGINT',stop);process.on('SIGTERM',stop);
