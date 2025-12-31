#!/usr/bin/env python3
"""
LLM-Driven SimpleBox Example

Demonstrates how to let an LLM explore a sandbox via tool calls:
- Create a SimpleBox sandbox
- Expose a sandbox_exec tool to run commands
- Let the model decide which commands to run
- Print a human-readable report
"""

import asyncio
import json
import os
from contextlib import AsyncExitStack

import boxlite
from openai import AsyncOpenAI

TOOLS = [
    {
        'type': 'function',
        'name': 'sandbox_exec',
        'description': 'Run a command inside the sandbox and return stdout/stderr/exit_code.',
        'parameters': {
            'type': 'object',
            'properties': {
                'argv': {
                    'type': 'array',
                    'items': {'type': 'string'},
                    'description': "Command and args, e.g. ['ls','-la'] or ['python','-c','print(123)']",
                }
            },
            'required': ['argv'],
        },
    }
]


def build_client():
    api_key = os.getenv('OPENAI_API_KEY')
    if not api_key:
        raise RuntimeError(
            "OPENAI_API_KEY is not set. Export it before running, e.g.: "
            "`export OPENAI_API_KEY=sk-...`"
        )
    return AsyncOpenAI(api_key=api_key)


async def sandbox_exec(box, argv):
    """
    argv: ["ls", "-la"] / ["python", "-c", "..."]
    """
    if not argv:
        return {'stdout': '', 'stderr': 'argv is required.', 'exit_code': 2}
    result = await box.exec(*argv)
    return {
        'stdout': result.stdout,
        'stderr': result.stderr,
        'exit_code': result.exit_code,
    }


async def whip_agent(box, client, user_goal, model='gpt-5.2', max_rounds=12):
    system_instructions = (
        'You are a powerful autonomous coding assistant.\n'
        'You can plan, explain, and iterate freely.\n'
        'When you need to interact with the environment, call sandbox_exec.\n'
        'Be careful and iterative; do not run destructive commands.\n'
        'Stop when you are done and summarize.\n'
    )

    print('\n[User Goal]\n', user_goal)

    response = await client.responses.create(
        model=model,
        instructions=system_instructions,
        input=[{'role': 'user', 'content': user_goal}],
        tools=TOOLS,
        tool_choice='auto',
    )

    for _ in range(max_rounds):
        for item in response.output:
            if item.type == 'message':
                for content in item.content:
                    if content.type in ('output_text', 'text'):
                        print('\n[LLM]\n', content.text)

        calls = [item for item in response.output if item.type == 'function_call']
        if not calls:
            return response
        else:
            call_info = '\n'.join(
                f"  -> name={call.name!r}, arguments={call.arguments!r}"
                for call in calls
            )
            print(f"\n[System] Executing tool calls: {call_info}")

        outputs = []
        for call in calls:
            try:
                args = json.loads(call.arguments or '{}')
            except Exception:
                args = {}

            argv = args.get('argv', [])
            if not isinstance(argv, list):
                out = {'stdout': '', 'stderr': 'Invalid argv; expected a list of strings.', 'exit_code': 2}
            else:
                out = await sandbox_exec(box, argv)

            call_id = getattr(call, 'call_id', None)
            if not call_id:
                raise RuntimeError(f'Tool call missing call_id: {call}')

            outputs.append({
                'type': 'function_call_output',
                'call_id': call_id,
                'output': json.dumps(out),
            })

        response = await client.responses.create(
            model=model,
            previous_response_id=response.id,
            input=outputs,
            tools=TOOLS,
            tool_choice='auto',
        )

    print('[System] max_rounds reached.')
    return response


async def main():
    client = build_client()

    stack = AsyncExitStack()
    box = await stack.enter_async_context(boxlite.SimpleBox(image='python:slim'))
    try:
        await whip_agent(
            box,
            client,
            'Explore this sandbox. Show python version, installed packages, $PATH and list files. '
            'Then run a short python snippet that prints system info. '
            'Finally give a human readable report.',
        )

        await whip_agent(
            box,
            client,
            'What commands (executables) are available in this sandbox? Show them all, split by commas.',
        )
    finally:
        await stack.aclose()


if __name__ == '__main__':
    asyncio.run(main())
