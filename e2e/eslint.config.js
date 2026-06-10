import { defineConfig, globalIgnores } from 'eslint/config';
import js from '@eslint/js';
import tseslint from 'typescript-eslint';

export default defineConfig(
	globalIgnores(['node_modules', 'test-results', 'playwright-report', '.fixtures', '.runtime']),
	js.configs.recommended,
	tseslint.configs.recommended,
	{
		rules: {
			'@typescript-eslint/consistent-type-imports': 'error',
			eqeqeq: ['error', 'smart'],
		},
	},
);
