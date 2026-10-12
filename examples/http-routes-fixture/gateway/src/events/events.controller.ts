import { Body, Controller, Delete, Get, Param, Patch, Post, Put } from '@nestjs/common';

@Controller('events')
export class EventsController {
  @Post()
  createEvent(@Body() dto: unknown) {
    return dto;
  }

  @Get(':id')
  getEvent(@Param('id') id: string) {
    return id;
  }

  @Patch(':id/state')
  changeEventState(@Param('id') id: string, @Body() dto: unknown) {
    return { id, dto };
  }

  @Put(':id/state')
  replaceEventState(@Param('id') id: string, @Body() dto: unknown) {
    return { id, dto };
  }

  @Get(':eventId/attendees')
  listAttendees(@Param('eventId') eventId: string) {
    return eventId;
  }

  @Delete(':id')
  removeEvent(@Param('id') id: string) {
    return id;
  }
}
