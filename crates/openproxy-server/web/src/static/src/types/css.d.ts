// types/css.d.ts — ambient module declaration for `*.css` imports.
//
// The build pipeline (build.mjs) configures esbuild's `loader: { '.css':
// 'text' }` so a `import x from './foo.css'` statement returns the file's
// contents as a string. TypeScript needs this ambient declaration to know
// that the import is valid and what type it produces — otherwise tsc fails
// with "Cannot find module './foo.css'" under the strict `Bundler` module
// resolution.
//
// We only use this for uPlot's bundled stylesheet at the moment
// (`uplot/dist/uPlot.min.css`); the wrapper injects it into the document
// via a <style> tag at first use so the chart canvas renders correctly
// without a separate <link> tag in index.html.

declare module '*.css' {
  const content: string;
  export default content;
}
