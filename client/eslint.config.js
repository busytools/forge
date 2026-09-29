import js from '@eslint/js';
import globals from 'globals';
import tseslint from 'typescript-eslint';
import svelte from 'eslint-plugin-svelte';

export default tseslint.config(
  // The shell's crate target dir holds generated .js (tauri-build's API script
  // and the codegen'd assets), which this project has no tsconfig for.
  { ignores: ['dist/', 'node_modules/', 'src-tauri/target/', 'src-tauri/gen/'] },
  js.configs.recommended,
  ...tseslint.configs.recommendedTypeChecked,
  ...svelte.configs['flat/recommended'],
  {
    // The svelte parser has to FORWARD the type-aware parser options, or every
    // type-aware rule throws on a .svelte file instead of linting it.
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: {
        parser: tseslint.parser,
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
        extraFileExtensions: ['.svelte'],
      },
    },
  },
  {
    languageOptions: {
      globals: globals.browser,
      parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname },
    },
    rules: {
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-non-null-assertion': 'error',
      '@typescript-eslint/ban-ts-comment': 'error',
      '@typescript-eslint/no-floating-promises': 'error',
      '@typescript-eslint/no-misused-promises': 'error',
      // A leading underscore is the convention for an argument that is
      // deliberately unused, which is how `rootTokens` ignores its palette
      // name while only one palette ships.
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_' }],
      'no-eval': 'error',
      'no-implied-eval': 'error',
      'svelte/no-at-html-tags': 'error',
    },
  },
  // LAST, because it has to override the blocks above rather than be
  // overridden by them: the client's own config files are Node scripts
  // outside the TypeScript project, so the project service refuses them and
  // every type-aware rule has nothing to read. The standard's type-aware half
  // is for the app.
  {
    files: ['*.config.js'],
    languageOptions: { globals: globals.node },
    ...tseslint.configs.disableTypeChecked,
  },
);
