// Compiles one HLSL file to a DXBC blob; the build script runs it under wine.

#include <windows.h>

#include <d3dcompiler.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
    if (argc != 4) {
        fprintf(stderr, "usage: shader_compiler <source> <profile> <output>\n");
        return 2;
    }
    const char *source_path = argv[1];
    const char *profile = argv[2];
    const char *output_path = argv[3];

    FILE *source_file = fopen(source_path, "rb");
    if (!source_file) {
        fprintf(stderr, "cannot open %s\n", source_path);
        return 1;
    }
    fseek(source_file, 0, SEEK_END);
    long length = ftell(source_file);
    rewind(source_file);
    char *source = malloc((size_t)length);
    if (!source || fread(source, 1, (size_t)length, source_file) != (size_t)length) {
        fprintf(stderr, "cannot read %s\n", source_path);
        return 1;
    }
    fclose(source_file);

    ID3DBlob *code = NULL;
    ID3DBlob *errors = NULL;
    HRESULT result = D3DCompile(source, (SIZE_T)length, source_path, NULL, NULL, "main", profile,
                                D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &code, &errors);
    if (errors) {
        fprintf(stderr, "%.*s\n", (int)errors->lpVtbl->GetBufferSize(errors),
                (const char *)errors->lpVtbl->GetBufferPointer(errors));
        errors->lpVtbl->Release(errors);
    }
    if (FAILED(result) || !code) {
        fprintf(stderr, "D3DCompile failed on %s with 0x%08lx\n", source_path, (unsigned long)result);
        return 1;
    }

    FILE *output_file = fopen(output_path, "wb");
    if (!output_file) {
        fprintf(stderr, "cannot write %s\n", output_path);
        return 1;
    }
    size_t size = code->lpVtbl->GetBufferSize(code);
    size_t written = fwrite(code->lpVtbl->GetBufferPointer(code), 1, size, output_file);
    fclose(output_file);
    code->lpVtbl->Release(code);
    if (written != size) {
        fprintf(stderr, "short write on %s\n", output_path);
        return 1;
    }
    return 0;
}
