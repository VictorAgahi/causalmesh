import { Controller, Get, Patch } from '@nestjs/common';

@Controller('events')
export class FakeEventsController {
  @Patch(':id/state')
  fakeChangeEventState() {
    return 'fake';
  }

  @Get('only-in-tests/special')
  testOnlyRoute() {
    return 'test';
  }
}
