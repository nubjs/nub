const react = { react: '19.2.8', 'react-dom': '19.2.8' };
const vite = { vite: '8.2.2', typescript: '6.0.3' };
export const marker = 'Framework build jail fixture';
const index = '<html><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>';
const data = `export const projects = [{ id: 1, name: '${marker}' }, { id: 2, name: 'Second project' }];`;
const files = { 'src/data.ts': data, 'src/style.css': 'body { font-family: system-ui; }' };
const staticApp = (name, dependencies, source, config, extraFiles = {}) => ({
  name, dependencies: { ...vite, ...dependencies }, build: 'vite build', output: 'dist',
  files: { ...files, 'index.html': index, 'src/main.tsx': source, 'vite.config.ts': config, ...extraFiles },
});

export const cases = [
  staticApp('vite-react', { ...react, '@vitejs/plugin-react': '6.1.1' },
    `import React, {useState} from 'react'; import {createRoot} from 'react-dom/client'; import {projects} from './data'; import './style.css'; function App(){const [count,setCount]=useState(0); return <main><h1>{projects[0].name}</h1><button onClick={()=>setCount(count+1)}>Count {count}</button>{projects.map(p=><p key={p.id}>{p.name}</p>)}</main>} createRoot(document.getElementById('root')!).render(<App/>);`,
    `import {defineConfig} from 'vite'; import react from '@vitejs/plugin-react'; export default defineConfig({plugins:[react()]});`),
  staticApp('vite-vue', { vue: '3.5.42', '@vitejs/plugin-vue': '6.0.8' },
    `import {createApp} from 'vue'; import App from './App.vue'; import './style.css'; createApp(App).mount('#root');`,
    `import {defineConfig} from 'vite'; import vue from '@vitejs/plugin-vue'; export default defineConfig({plugins:[vue()]});`,
    { 'src/App.vue': `<script setup lang="ts">import {ref} from 'vue'; import {projects} from './data'; const count=ref(0);</script><template><main><h1>{{projects[0].name}}</h1><button @click="count++">Count {{count}}</button><p v-for="project in projects" :key="project.id">{{project.name}}</p></main></template>` }),
  staticApp('vite-solid', { 'solid-js': '1.9.15', 'vite-plugin-solid': '2.11.14' },
    `import {render} from 'solid-js/web'; import {createSignal,For} from 'solid-js'; import {projects} from './data'; import './style.css'; function App(){const [count,setCount]=createSignal(0); return <main><h1>{projects[0].name}</h1><button onClick={()=>setCount(count()+1)}>Count {count()}</button><For each={projects}>{p=><p>{p.name}</p>}</For></main>} render(()=><App/>,document.getElementById('root')!);`,
    `import {defineConfig} from 'vite'; import solid from 'vite-plugin-solid'; export default defineConfig({plugins:[solid()]});`),
  staticApp('qwik', { '@builder.io/qwik': '1.20.0', vite: '7.3.1' },
    `import {component$,useSignal,render} from '@builder.io/qwik'; import {projects} from './data'; const App=component$(()=>{const count=useSignal(0); return <main><h1>{projects[0].name}</h1><button onClick$={()=>count.value++}>Count {count.value}</button></main>}); render(document.getElementById('root')!,<App/>);`,
    `import {defineConfig} from 'vite'; import {qwikVite} from '@builder.io/qwik/optimizer'; export default defineConfig({plugins:[qwikVite({csr:true})]});`),
  {
    name: 'next', dependencies: { ...react, next: '16.3.4', sharp: '0.35.4', typescript: '6.0.3', '@types/node': '26.4.1', '@types/react': '19.2.18', '@types/react-dom': '19.2.7' },
    build: 'next build', start: 'next start', output: '.next', native: true,
    files: {
      'app/layout.tsx': `export default function Layout({children}) {return <html><body>{children}</body></html>}`,
      'app/page.tsx': `import Link from 'next/link'; export default function Page(){return <main><h1>${marker}</h1><Link href="/projects/1">Project</Link></main>}`,
      'app/projects/[id]/page.tsx': `export default async function Page({params}){const {id}=await params; return <h1>Project {id}</h1>}`,
      'app/api/projects/route.ts': `export function GET(){return Response.json([{id:1,name:'${marker}'}])}`,
      'next.config.mjs': 'export default {typescript:{ignoreBuildErrors:false},experimental:{cpus:2}};',
      'tsconfig.json': JSON.stringify({ compilerOptions: { target: 'ES2020', lib: ['dom', 'esnext'], allowJs: true, skipLibCheck: true, strict: false, noEmit: true, module: 'esnext', moduleResolution: 'bundler', jsx: 'react-jsx', plugins: [{ name: 'next' }] }, include: ['**/*.ts', '**/*.tsx', '.next/types/**/*.ts'] }),
    },
  },
  {
    name: 'nuxt', dependencies: { nuxt: '4.5.2', vue: '3.5.42', typescript: '6.0.3' },
    prepare: 'nuxt prepare', build: 'nuxt build', start: 'node .output/server/index.mjs', output: '.output',
    files: {
      'nuxt.config.ts': `export default defineNuxtConfig({compatibilityDate:'2026-09-06',devtools:{enabled:false}});`,
      'app/app.vue': '<template><NuxtPage /></template>',
      'app/pages/index.vue': `<script setup>const {data}=await useFetch('/api/projects')</script><template><main><h1>${marker}</h1><p v-for="project in data" :key="project.id">{{project.name}}</p></main></template>`,
      'server/api/projects.get.ts': `export default defineEventHandler(()=>[{id:1,name:'${marker}'}]);`,
    },
  },
  {
    name: 'sveltekit', dependencies: { ...vite, svelte: '5.57.0', '@sveltejs/kit': '2.70.3', '@sveltejs/adapter-node': '5.5.7', '@sveltejs/vite-plugin-svelte': '7.3.0' },
    prepare: 'svelte-kit sync', build: 'vite build', start: 'node build', output: 'build',
    files: {
      'vite.config.ts': `import {sveltekit} from '@sveltejs/kit/vite'; import {defineConfig} from 'vite'; export default defineConfig({plugins:[sveltekit()]});`,
      'svelte.config.js': `import adapter from '@sveltejs/adapter-node'; export default {kit:{adapter:adapter()}};`,
      'src/app.html': '<!doctype html><html><head>%sveltekit.head%</head><body><div>%sveltekit.body%</div></body></html>',
      'src/routes/+page.server.ts': `export const load=()=>({projects:[{id:1,name:'${marker}'}]});`,
      'src/routes/+page.svelte': '<script>let {data}=$props();</script><main>{#each data.projects as project}<h1>{project.name}</h1>{/each}</main>',
    },
  },
  {
    name: 'astro', dependencies: { astro: '7.3.1', '@astrojs/react': '6.0.5', ...react, sharp: '0.35.4' },
    build: 'astro build', output: 'dist', native: true,
    files: {
      'astro.config.mjs': `import {defineConfig} from 'astro/config'; import react from '@astrojs/react'; export default defineConfig({integrations:[react()]});`,
      'src/components/Counter.jsx': `import {useState} from 'react'; export default function Counter(){const [count,setCount]=useState(0); return <button onClick={()=>setCount(count+1)}>Count {count}</button>}`,
      'src/pages/index.astro': `---\nimport Counter from '../components/Counter.jsx';\nconst projects=[{id:1,name:'${marker}'}];\n---\n<html><body><main>{projects.map(p=><h1>{p.name}</h1>)}<Counter client:load /></main></body></html>`,
      'src/pages/projects/[id].astro': `---\nexport function getStaticPaths(){return [{params:{id:'1'}}]}\n---\n<h1>Project {Astro.params.id}</h1>`,
    },
  },
  {
    name: 'react-router', dependencies: { ...vite, ...react, isbot: '5.2.2', 'react-router': '8.3.1', '@react-router/dev': '8.3.1', '@react-router/node': '8.3.1', '@react-router/serve': '8.3.1' },
    build: 'react-router build', start: 'react-router-serve build/server/index.js', output: 'build',
    files: {
      'vite.config.ts': `import {reactRouter} from '@react-router/dev/vite'; import {defineConfig} from 'vite'; export default defineConfig({plugins:[reactRouter()]});`,
      'app/routes.ts': `import {index} from '@react-router/dev/routes'; export default [index('routes/home.tsx')];`,
      'app/root.tsx': `import {Links,Meta,Outlet,Scripts,ScrollRestoration} from 'react-router'; export function Layout({children}){return <html><head><Meta/><Links/></head><body>{children}<ScrollRestoration/><Scripts/></body></html>} export default function App(){return <Outlet/>}`,
      'app/routes/home.tsx': `import {useLoaderData} from 'react-router'; export function loader(){return {name:'${marker}'}} export default function Home(){const data=useLoaderData(); return <h1>{data.name}</h1>}`,
    },
  },
  {
    name: 'solid-start', dependencies: { ...vite, '@solidjs/start': '2.0.4', 'solid-js': '1.9.15', '@solidjs/router': '1.0.0', nitro: '3.0.260903-beta' },
    build: 'vite build', start: 'node .output/server/index.mjs', output: '.output',
    files: {
      'vite.config.ts': `import {defineConfig} from 'vite'; import {solidStart} from '@solidjs/start/config'; import {nitro} from 'nitro/vite'; export default defineConfig({plugins:[solidStart(),nitro()]});`,
      'src/app.tsx': `import {createSignal} from 'solid-js'; export default function App(){const [count,setCount]=createSignal(0); return <main><h1>${marker}</h1><button onClick={()=>setCount(count()+1)}>Count {count()}</button></main>}`,
      'src/entry-client.tsx': `import {mount,StartClient} from '@solidjs/start/client'; mount(()=><StartClient/>,document.getElementById('app')!);`,
      'src/entry-server.tsx': `import {createHandler,StartServer} from '@solidjs/start/server'; export default createHandler(()=><StartServer document={({assets,children,scripts})=><html><head>{assets}</head><body><div id="app">{children}</div>{scripts}</body></html>}/>);`,
    },
  },
  {
    name: 'expo', dependencies: { react: '19.2.3', 'react-dom': '19.2.3', expo: '57.0.20', 'react-native': '0.86.3', 'react-native-web': '0.21.2', '@expo/metro-runtime': '57.0.15' },
    build: 'expo export --platform web', output: 'dist',
    files: {
      'index.js': `import {registerRootComponent} from 'expo'; import App from './App'; registerRootComponent(App);`,
      'App.js': `import {useState} from 'react'; import {View,Text,Button} from 'react-native'; export default function App(){const [count,setCount]=useState(0); return <View><Text>${marker}</Text><Button title={'Count '+count} onPress={()=>setCount(count+1)}/></View>}`,
      'app.json': JSON.stringify({ expo: { name: 'Framework fixture', slug: 'framework-fixture', web: { bundler: 'metro' } } }),
    },
  },
  {
    name: 'angular', dependencies: { '@angular/core': '22.1.5', '@angular/common': '22.1.5', '@angular/compiler': '22.1.5', '@angular/compiler-cli': '22.1.5', '@angular/platform-browser': '22.1.5', '@angular/build': '22.1.7', '@angular/cli': '22.1.7', rxjs: '7.8.2', typescript: '6.0.3', tslib: '2.8.1' },
    build: 'ng build', output: 'dist/app/browser',
    files: {
      'angular.json': JSON.stringify({ version: 1, projects: { app: { projectType: 'application', root: '', sourceRoot: 'src', architect: { build: { builder: '@angular/build:application', options: { browser: 'src/main.ts', index: 'src/index.html', tsConfig: 'tsconfig.json', outputPath: 'dist/app' } } } } } }),
      'tsconfig.json': JSON.stringify({ compilerOptions: { target: 'ES2022', module: 'preserve', moduleResolution: 'bundler', experimentalDecorators: true, skipLibCheck: true, strict: true }, angularCompilerOptions: { strictTemplates: true }, include: ['src/**/*.ts'] }),
      'src/index.html': '<html><head><base href="/"></head><body><app-root></app-root></body></html>',
      'src/main.ts': `import {Component,signal} from '@angular/core'; import {bootstrapApplication} from '@angular/platform-browser'; @Component({selector:'app-root',standalone:true,template:'<h1>${marker}</h1><button (click)="count.set(count()+1)">Count {{count()}}</button>'}) class App{count=signal(0)} bootstrapApplication(App);`,
    },
  },
  ...['express', 'fastify', 'hono', 'nest'].map(name => ({
    name, dependencies: { esbuild: '0.28.2', sharp: '0.35.4', ...(name === 'express' ? { express: '5.2.1' } : name === 'fastify' ? { fastify: '5.12.3' } : name === 'hono' ? { hono: '4.13.7', '@hono/node-server': '2.1.1' } : { '@nestjs/core': '12.0.1', '@nestjs/common': '12.0.1', '@nestjs/platform-fastify': '12.0.1', 'reflect-metadata': '0.2.2', rxjs: '7.8.2' }) },
    build: 'esbuild server.mjs --platform=node --format=esm --packages=external --outfile=dist/server.mjs', start: 'node dist/server.mjs', output: 'dist', native: true,
    files: { 'server.mjs': name === 'express'
      ? `import express from 'express'; const app=express(); app.get('/',(_req,res)=>res.json({name:'${marker}'})); app.listen(Number(process.env.PORT),'127.0.0.1');`
      : name === 'fastify'
      ? `import Fastify from 'fastify'; const app=Fastify(); app.get('/',()=>({name:'${marker}'})); await app.listen({port:Number(process.env.PORT),host:'127.0.0.1'});`
      : name === 'hono'
        ? `import {Hono} from 'hono'; import {serve} from '@hono/node-server'; const app=new Hono(); app.get('/',c=>c.json({name:'${marker}'})); serve({fetch:app.fetch,port:Number(process.env.PORT),hostname:'127.0.0.1'});`
        : `import 'reflect-metadata'; import {Controller,Get,Module} from '@nestjs/common'; import {NestFactory} from '@nestjs/core'; import {FastifyAdapter} from '@nestjs/platform-fastify'; class Projects{list(){return {name:'${marker}'}}} Controller()(Projects); Get()(Projects.prototype,'list',Object.getOwnPropertyDescriptor(Projects.prototype,'list')); class App{} Module({controllers:[Projects]})(App); const app=await NestFactory.create(App,new FastifyAdapter()); await app.listen(Number(process.env.PORT),'127.0.0.1');` },
  })),
];
